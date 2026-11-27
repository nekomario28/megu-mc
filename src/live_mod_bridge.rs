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
//     / `*:add_table` loot modifiers, food specs), and lifted bindings
//     subscribe to standard events immediately after boot.
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
const LIVE_MOD_S2C_CUSTOM_PAYLOAD_ID: i64 = 0x19;

/// Feature-independent mirror of
/// `mod_adapter_core::world_extensions::ProgressionVerdict`, so
/// `live_check_progression_barrier` keeps one signature (and one return
/// type) with or without `mod-suite`.
#[cfg(any(test, feature = "mod-suite"))]
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
#[allow(clippy::too_many_arguments)]
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
    let decision = bridge
        .crowd
        .decide(class, entity_id, chunk_x, chunk_z, tick);
    debug_assert!(decision.run_physics_and_damage());
    decision.run_idle_ai()
}

/// Cache the compressed chunk payload across recipients at the same epoch.
#[cfg(feature = "mod-suite")]
pub fn live_chunk_packet_cache_get_or_encode<F>(
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
pub fn live_chunk_packet_cache_invalidate(dim: i32, cx: i32, cz: i32) -> usize {
    live_server_perf_bridge()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .chunks
        .invalidate_chunk(dim, cx, cz)
}

#[cfg(not(feature = "mod-suite"))]
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

// ===========================================================================
// mod-suite implementation
// ===========================================================================

#[cfg(feature = "mod-suite")]
use mod_adapter_core::auto_adapter::{
    auto_adapt_jar, auto_adapt_mods_dir, AdaptationTier, AutoAdaptOptions, JarAdaptationResult,
    ModsDirAdaptationReport, MAX_MODS_DIR_AGGREGATE_BYTES,
};
#[cfg(feature = "mod-suite")]
use mod_adapter_core::bytecode_subset_vm::{
    execute_verified_method, parse_class_methods, verify_subset_method, HostCallKind, SubsetVmHost,
    VerifiedMethod, VmError, VmValue, DEFAULT_FUEL,
};
#[cfg(feature = "mod-suite")]
use mod_adapter_core::capability_bus::{
    BlockCapabilityBus, BlockCoord, CapabilityFluidStack, CapabilityItemStack,
    CookingPotItemAdapter, FaceDirection, ItemTransferHandler, SimpleEnergyBuffer, SimpleFluidTank,
    SimpleSlotInventory,
};
#[cfg(feature = "mod-suite")]
use mod_adapter_core::colossal_and_forge::{
    evaluate_breath_cone, BlockProbe, BreathEntity, BreathOutcome, ElementKind,
    ElementalBreathSpec, ForgeCellExpect, ForgePatternCell, ForgeTickEvents, GrowthStageCurve,
    MultiBlockForgeRuntime, MultiBlockForgeSpec, MultiPartHitbox, PartHit, PartKind,
    PartOffsetSpec,
};
#[cfg(feature = "mod-suite")]
use mod_adapter_core::culinary_adapter::{
    is_heated, CookResult, CookingPotState, CulinaryDiscoveryStats, CulinaryRegistry,
    CuttingOutcome, IngredientMatcher, TagIndex,
};
#[cfg(feature = "mod-suite")]
use mod_adapter_core::ecosystem_compat::{
    plan_dual_loader_mods_dir, AccessoryError, AccessoryInventory, AccessoryItem, AttributeTotals,
    DualLoaderPlan, MissingDependencyPlan, ModDependencyKind, ModEnvironment,
};
#[cfg(feature = "mod-suite")]
use mod_adapter_core::live_registry_bridge::{
    DynamicBlockStateEntry, RegistryContentIndex, UnifiedModRegistryPlan, MOD_REGISTRY_SYNC_CHANNEL,
};
#[cfg(feature = "mod-suite")]
use mod_adapter_core::loot_modifier_engine::{
    detect_bytecode_loot_hooks, BytecodeLootHookReport, GlobalLootModifierEngine, LootDataEntry,
    LootDataLayer, LootDropItem, LootLayerKind, LootModifierContext, ModLootOverlayIndex,
};
#[cfg(feature = "mod-suite")]
use mod_adapter_core::magic_primitives as lmb_magic;
#[cfg(feature = "mod-suite")]
use mod_adapter_core::mixin_lifter::{LiftedBinding, MixinLiftVerdict, StandardEventHook};
#[cfg(feature = "mod-suite")]
use mod_adapter_core::mod_sync as lmb_sync;
#[cfg(feature = "mod-suite")]
use mod_adapter_core::provenance_diff::{
    DiffDomain, ProvenanceAction, ProvenanceDiffLedger, ReloadDiffSummary,
};
#[cfg(feature = "mod-suite")]
use mod_adapter_core::recipe_conflict_unify::{
    InputSignature, MaterialUnifier, RecipeCandidate, RecipeConflictIndex,
};
#[cfg(feature = "mod-suite")]
use mod_adapter_core::season_primitives::{
    encode_ssn1, seasonal_foliage_tint, seasonal_grass_tint, BiomeClimateClass, BiomeClimateRules,
    SeasonCalendarConfig, SeasonState, SeasonSyncState, SeasonalTintRules, SSN1_FRAME_LEN,
};
#[cfg(feature = "mod-suite")]
use mod_adapter_core::unified_bundle::{DeclarativeModBundle, UnifiedModRuntime};
#[cfg(feature = "mod-suite")]
use mod_adapter_core::unified_tags::{canonical_common_tag_id, TagDomain, UnifiedTagIndex};
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
#[cfg(feature = "mod-suite")]
use mod_offload::auto_optimizer::{
    fingerprint_item_slots, pack_machine_key, AdaptivePollBackoff, AutoBulkSectionBatcher,
    AutoDeltaCoalescer, AutoRecipeMemoizer, BulkSectionBatch, DistanceTier, EmitDecision,
};
#[cfg(feature = "mod-suite")]
use mod_offload::lazy_catchup::{
    catch_up_furnace, catch_up_growth_stage, catch_up_machine_batch, CatchUpLimits,
    FurnaceCatchUpReport, FurnaceCatchUpState, HeadlessFactoryNode, HeadlessFactoryRegistry,
    MachineCatchUpReport, MachineCatchUpState, MILLI_PER_ITEM,
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
        assert!(live_should_tick_crowd_mob_ai(
            entity_id, cx, cz, 0, 40, true
        ));
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
        let reencoded = live_chunk_packet_cache_get_or_encode(dim, cx, cz, 1, 0, || vec![6, 7]);
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

#[cfg(feature = "mod-suite")]
type LiveAdaptedJar = (String, Result<JarAdaptationResult, String>);

#[cfg(feature = "mod-suite")]
type LiveLootSourceTexts = BTreeMap<(String, String), String>;

#[cfg(feature = "mod-suite")]
impl LiveModJarReport {
    fn from_result(file_name: &str, result: &JarAdaptationResult, diagnostic: String) -> Self {
        let scan = result.mixin_scan.as_ref();
        Self {
            file_name: file_name.to_string(),
            mod_id: result
                .mod_id()
                .map_or_else(|| live_mod_file_stem(file_name), str::to_string),
            tier: result.tier,
            diagnostic,
            classes_scanned: scan.map_or(0, |s| s.classes_scanned),
            mixin_sites: scan.map_or(0, |s| s.sites.len()),
            skipped_client_mixins: scan.map_or(0, |s| s.skipped_client_mixins),
        }
    }
}

/// What was automatically onboarded from a JAR. Lifted bindings are installed
/// only for a fully verified [`AdaptationTier::Tier2LiftedShim`]
/// classification; Tier 3 is diagnostic and auto-skipped JARs record no
/// summary at all.
#[cfg(feature = "mod-suite")]
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
    /// Per-standard-hook dispatch counters — proof a registered binding
    /// actually fired on a live path (diagnostics + tests).
    hook_fires: BTreeMap<StandardEventHook, u64>,
    /// Validated merged Fabric + NeoForge server-side manifest table.
    dual_loader: DualLoaderPlan,
    /// Methods verified against the restricted JVM opcode and host-call sets.
    damage_hooks: Vec<VerifiedMethod>,
    /// Merged Farmer's Delight + addon culinary content across all loaded
    /// JARs (cooking/cutting recipes, tags, loot modifiers, food specs).
    culinary: CulinaryRegistry,
    /// Aggregated discovery counters over `culinary` (for the boot report).
    culinary_stats: CulinaryDiscoveryStats,
    /// Live pots are keyed by dimension and block position. Each pot lives
    /// inside the same `CookingPotItemAdapter` the shared capability bus
    /// points at (see `LiveSharedItemEndpoint`), so items a Create duct or
    /// a foreign mod pushes through the bus land in the canonical state
    /// the deadline scheduler advances — never a shadow copy.
    cooking_pots: HashMap<(DimId, i32, i32, i32), Arc<Mutex<CookingPotItemAdapter>>>,
    /// One merged `#c:`/`#forge:`/`#neoforge:` tag index over every
    /// scanned mod JAR (Jar-in-Jar members folded in), seeded with the
    /// Vanilla 1.21.1 `c:` baseline. `live_unified_tag_matches` answers
    /// cross-mod membership probes off it, and `scan_mods_dir` folds its
    /// flattened contents back into `culinary.tags` so nested-tag recipes
    /// match uniformly on either spelling.
    unified_tags: UnifiedTagIndex,
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
    /// Content generation for the MRS1-synced registry inputs, bumped by
    /// `scan_mods_dir` — the only production writer of `registry_plan`.
    /// Folded into the join-frame cache key so a join after a rescan never
    /// replays a stale registry.
    registry_revision: u64,
    /// One cached `MRS1` frame keyed by the registry inputs it was built
    /// from. `None` until the first join builds it, and after any
    /// invalidation — subsequent joins hand the same `Arc` back in O(1).
    mrs1_cache: Option<Mrs1FrameCache>,
    /// OreDictionary-style recipe de-confliction surface, rebuilt at the end
    /// of every `scan_mods_dir`: overlapping culinary recipes register under
    /// their input signature so `resolve` ranks them deterministically
    /// (vanilla first, then recipe id).
    recipe_conflicts: RecipeConflictIndex,
    /// Canonicalises equivalent material ids through the merged unified tag
    /// index. Disabled by default for strict Vanilla parity; consulted by
    /// the live recipe lookup surface.
    material_unifier: MaterialUnifier,
    /// Solver-admitted `mods/` file names. Only these JARs may publish any
    /// registry, tag, recipe, loot or hook content.
    admitted_jars: BTreeSet<String>,
    /// Solver-admitted primary mod ids, in the same admission decision.
    admitted_mod_ids: BTreeSet<String>,
    /// Structured dependency-resolution diagnostics surfaced to operators.
    dependency_resolution: MissingDependencyPlan,
    /// Winning mod loot-table JSON by canonical table id (world datapacks
    /// remain a higher overlay layer at publish time).
    loot_tables_raw: BTreeMap<String, String>,
    loot_overlay: ModLootOverlayIndex,
    glm_engine: GlobalLootModifierEngine,
    loot_provenance: ProvenanceDiffLedger,
    previous_loot_provenance: ProvenanceDiffLedger,
    loot_reload_diff: ReloadDiffSummary,
    loot_diagnostics: Vec<String>,
    bytecode_loot_hooks: Vec<BytecodeLootHookReport>,
    addon_unsupported_serializers: BTreeSet<String>,
}

/// The inputs one `MRS1` frame is a pure function of: the suite's registry
/// revision, whether Create is armed, and the shape of every contributing
/// table. Cheap to recompute — comparing this key is the whole cache-hit
/// cost.
#[cfg(feature = "mod-suite")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Mrs1FrameKey {
    registry_revision: u64,
    create_enabled: bool,
    create_state_count: usize,
    plan_state_count: usize,
    plan_item_count: usize,
}

#[cfg(feature = "mod-suite")]
#[derive(Clone, Debug)]
struct Mrs1FrameCache {
    key: Mrs1FrameKey,
    frame: Option<Arc<[u8]>>,
}

#[cfg(feature = "mod-suite")]
fn live_mod_result_verified(result: &JarAdaptationResult) -> bool {
    matches!(
        result.tier,
        AdaptationTier::NativeBundle | AdaptationTier::Tier2LiftedShim
    )
}

#[cfg(feature = "mod-suite")]
fn strict_dependency_admission(
    plan: &DualLoaderPlan,
) -> (BTreeSet<String>, BTreeSet<String>, Vec<String>) {
    let mut blocked = BTreeSet::new();
    for missing in &plan.missing_dependencies {
        blocked.insert(missing.mod_id.clone());
    }
    for mismatch in &plan.version_mismatches {
        blocked.insert(mismatch.mod_id.clone());
    }
    for conflict in &plan.dependency_conflicts {
        if conflict.kind == ModDependencyKind::Incompatible {
            blocked.insert(conflict.mod_id.clone());
        }
    }
    for cycle in &plan.dependency_cycles {
        blocked.extend(cycle.members.iter().cloned());
    }
    loop {
        let before = blocked.len();
        for planned in &plan.unified_mods {
            if blocked.contains(&planned.mod_id) {
                continue;
            }
            let depends_on_blocked = planned.dependencies.iter().any(|dep| {
                dep.solver_required()
                    && dep.side != ModEnvironment::Client
                    && blocked.contains(&dep.mod_id)
            });
            if depends_on_blocked {
                blocked.insert(planned.mod_id.clone());
            }
        }
        if blocked.len() == before {
            break;
        }
    }
    let mut jars = BTreeSet::new();
    let mut ids = BTreeSet::new();
    let mut diagnostics = Vec::new();
    for planned in &plan.unified_mods {
        if blocked.contains(&planned.mod_id) {
            diagnostics.push(format!(
                "{} ({}) blocked: required dependency closure is incomplete or unsafe",
                planned.mod_id, planned.jar_name
            ));
        } else {
            jars.insert(planned.jar_name.clone());
            ids.insert(planned.mod_id.clone());
        }
    }
    (jars, ids, diagnostics)
}

#[cfg(feature = "mod-suite")]
fn live_loot_table_id_from_path(path: &str) -> Option<String> {
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() < 4 || parts[0] != "data" {
        return None;
    }
    if parts[2] != "loot_table" && parts[2] != "loot_tables" {
        return None;
    }
    let stem = parts[3..].join("/");
    let stem = stem.strip_suffix(".json")?;
    Some(format!("{}:{stem}", parts[1]))
}

#[cfg(feature = "mod-suite")]
fn live_zip_field<const N: usize>(jar: &[u8], offset: usize) -> Option<[u8; N]> {
    jar.get(offset..offset.checked_add(N)?)?.try_into().ok()
}

#[cfg(feature = "mod-suite")]
fn live_zip_directory(jar: &[u8]) -> Result<(usize, usize, usize), &'static str> {
    let u16_at = |p| live_zip_field(jar, p).map(u16::from_le_bytes);
    let u32_at = |p| live_zip_field(jar, p).map(u32::from_le_bytes);
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
    Ok((directory, directory + directory_len, count))
}

#[cfg(feature = "mod-suite")]
struct LiveZipMember<'a> {
    name: &'a [u8],
    flags: u16,
    method: u16,
    crc: u32,
    compressed: usize,
    expanded: usize,
    local: usize,
}

#[cfg(feature = "mod-suite")]
#[derive(Clone, Copy)]
enum LiveZipKind {
    Loot,
    Class,
}

#[cfg(feature = "mod-suite")]
impl LiveZipKind {
    fn error(self, loot: &'static str, class: &'static str) -> &'static str {
        match self {
            Self::Loot => loot,
            Self::Class => class,
        }
    }
}

#[cfg(feature = "mod-suite")]
impl<'a> LiveZipMember<'a> {
    fn read(
        jar: &'a [u8],
        cursor: &mut usize,
        end: usize,
        kind: LiveZipKind,
    ) -> Result<Self, &'static str> {
        let u16_at = |p| live_zip_field(jar, p).map(u16::from_le_bytes);
        let u32_at = |p| live_zip_field(jar, p).map(u32::from_le_bytes);
        let p = *cursor;
        if u32_at(p) != Some(0x0201_4b50) {
            return Err("invalid zip central member");
        }
        let flags = u16_at(p + 8).ok_or("zip flags")?;
        let method = u16_at(p + 10).ok_or("zip method")?;
        let crc = match kind {
            LiveZipKind::Class => u32_at(p + 16).ok_or("zip crc")?,
            LiveZipKind::Loot => 0,
        };
        let compressed = usize::try_from(u32_at(p + 20).ok_or("zip compressed size")?)
            .map_err(|_| "zip compressed length overflow")?;
        let expanded = usize::try_from(u32_at(p + 24).ok_or("zip expanded size")?)
            .map_err(|_| "zip expanded length overflow")?;
        let name_len = usize::from(u16_at(p + 28).ok_or("zip name size")?);
        let extra_len = usize::from(u16_at(p + 30).ok_or("zip extra size")?);
        let comment_len = usize::from(u16_at(p + 32).ok_or("zip comment size")?);
        let local = usize::try_from(u32_at(p + 42).ok_or("zip local offset")?)
            .map_err(|_| "zip local offset overflow")?;
        let name_start = p.checked_add(46).ok_or("zip header overflow")?;
        let name_end = name_start
            .checked_add(name_len)
            .ok_or("zip name overflow")?;
        let name = jar.get(name_start..name_end).ok_or("zip name truncated")?;
        *cursor = name_end
            .checked_add(extra_len)
            .and_then(|p| p.checked_add(comment_len))
            .ok_or("zip member overflow")?;
        if *cursor > end {
            return Err("zip central member truncated");
        }
        Ok(Self {
            name,
            flags,
            method,
            crc,
            compressed,
            expanded,
            local,
        })
    }

    fn check_bounds(
        &self,
        count: usize,
        max_members: usize,
        max_member_bytes: usize,
        kind: LiveZipKind,
    ) -> Result<(), &'static str> {
        if count >= max_members || self.expanded > max_member_bytes {
            return Err(kind.error(
                "loot member count or size bound exceeded",
                "class count or size bound exceeded",
            ));
        }
        if self.flags & 1 != 0 || (self.method != 0 && self.method != 8) {
            return Err(kind.error(
                "encrypted or unsupported compressed loot member",
                "encrypted or unsupported compressed class",
            ));
        }
        Ok(())
    }

    fn decode(
        &self,
        jar: &[u8],
        name: &str,
        max_member_bytes: usize,
        kind: LiveZipKind,
    ) -> Result<Vec<u8>, &'static str> {
        use std::io::Read;

        if name.starts_with('/') || name.split('/').any(|segment| segment == "..") {
            return Err(kind.error("unsafe loot member name", "unsafe class member name"));
        }
        let local = self.local;
        if live_zip_field(jar, local).map(u32::from_le_bytes) != Some(0x0403_4b50) {
            return Err(kind.error("invalid local loot header", "invalid local class header"));
        }
        let u16_at = |p| live_zip_field(jar, p).map(u16::from_le_bytes);
        let start = local
            .checked_add(30)
            .and_then(|p| p.checked_add(usize::from(u16_at(local + 26)?)))
            .and_then(|p| p.checked_add(usize::from(u16_at(local + 28)?)))
            .ok_or(kind.error(
                "loot payload offset overflow",
                "class payload offset overflow",
            ))?;
        let end = start
            .checked_add(self.compressed)
            .ok_or(kind.error("loot payload overflow", "class payload overflow"))?;
        let payload = jar
            .get(start..end)
            .ok_or(kind.error("loot payload truncated", "class payload truncated"))?;
        if self.method == 0 {
            return Ok(payload.to_vec());
        }
        let mut out = Vec::new();
        flate2::read::DeflateDecoder::new(payload)
            .take((max_member_bytes + 1) as u64)
            .read_to_end(&mut out)
            .map_err(|_| {
                kind.error(
                    "invalid loot deflate stream",
                    "invalid class deflate stream",
                )
            })?;
        Ok(out)
    }

    fn account_decoded(
        &self,
        len: usize,
        total: &mut usize,
        max_total: usize,
        kind: LiveZipKind,
    ) -> Result<(), &'static str> {
        *total = total
            .checked_add(len)
            .ok_or(kind.error("loot total size overflow", "class total size overflow"))?;
        if len != self.expanded || *total > max_total {
            return Err(kind.error(
                "loot expanded size mismatch or total bound exceeded",
                "class expanded size mismatch or total bound exceeded",
            ));
        }
        Ok(())
    }
}

#[cfg(feature = "mod-suite")]
fn live_mod_zip_members(
    jar: &[u8],
    include: &dyn Fn(&str) -> bool,
    max_members: usize,
    max_member_bytes: usize,
    max_total_bytes: usize,
) -> Result<Vec<LootDataEntry>, &'static str> {
    let (mut cursor, end, count) = live_zip_directory(jar)?;
    let mut decoded_total = 0;
    let mut entries = Vec::new();
    let kind = LiveZipKind::Loot;
    for _ in 0..count {
        let member = LiveZipMember::read(jar, &mut cursor, end, kind)?;
        let Ok(name) = std::str::from_utf8(member.name) else {
            continue;
        };
        if !include(name) {
            continue;
        }
        member.check_bounds(entries.len(), max_members, max_member_bytes, kind)?;
        let decoded = member.decode(jar, name, max_member_bytes, kind)?;
        member.account_decoded(decoded.len(), &mut decoded_total, max_total_bytes, kind)?;
        entries.push(LootDataEntry::new(name.to_string(), decoded));
    }
    if cursor != end {
        return Err("zip directory size mismatch");
    }
    Ok(entries)
}

#[cfg(feature = "mod-suite")]
fn live_namespaced_item_name(name: &str) -> String {
    if name.contains(':') {
        name.to_string()
    } else {
        format!("minecraft:{name}")
    }
}

#[cfg(feature = "mod-suite")]
fn live_mod_with_registry_plan<R>(
    resolve: impl FnOnce(&UnifiedModRegistryPlan) -> Option<R>,
) -> Option<R> {
    let suite = live_mod_suite().lock().unwrap_or_else(|e| e.into_inner());
    resolve(suite.registry_plan.as_ref()?)
}

#[cfg(feature = "mod-suite")]
fn live_mod_registry_item_id(name: &str) -> Option<i64> {
    let canonical = live_namespaced_item_name(name);
    live_mod_with_registry_plan(|plan| {
        plan.items
            .iter()
            .find(|(_, item)| **item == canonical)
            .map(|(id, _)| i64::from(*id))
    })
}

#[cfg(feature = "mod-suite")]
fn live_mod_registry_item_name(item: i64) -> Option<String> {
    let id = u32::try_from(item).ok()?;
    live_mod_with_registry_plan(|plan| plan.resolve_item(id).map(str::to_string))
}

#[cfg(feature = "mod-suite")]
fn inject_live_mod_loot_underlay(content: &mut DatapackContent, tables: &BTreeMap<String, String>) {
    for (key, body) in tables {
        if content.loot_raw.contains_key(key) {
            continue;
        }
        let rel = key.split_once(':').map(|(_, rel)| rel).unwrap_or(key);
        content.read_loot(key.clone(), rel, body);
    }
}

#[cfg(feature = "mod-suite")]
fn apply_live_mod_loot_underlay_to_content(content: &mut DatapackContent) {
    let tables = live_mod_suite()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .loot_tables_raw
        .clone();
    inject_live_mod_loot_underlay(content, &tables);
}

#[cfg(not(feature = "mod-suite"))]
#[inline(always)]
fn apply_live_mod_loot_underlay_to_content(_content: &mut DatapackContent) {}

#[cfg(feature = "mod-suite")]
fn publish_live_mod_loot_underlay(tables: &BTreeMap<String, String>) {
    if tables.is_empty() {
        return;
    }
    let mut content = DatapackContent::default();
    inject_live_mod_loot_underlay(&mut content, tables);
    let mut loot = loot_tables().lock().unwrap_or_else(|e| e.into_inner());
    let mut raw = raw_loot_tables().lock().unwrap_or_else(|e| e.into_inner());
    let mut mob = mob_loot_tables().lock().unwrap_or_else(|e| e.into_inner());
    for (key, body) in content.loot_raw {
        raw.entry(key).or_insert(body);
    }
    for (key, pools) in content.loot {
        loot.entry(key).or_insert(pools);
    }
    for (etype, pools) in content.mob_loot {
        mob.entry(etype).or_insert(pools);
    }
}

#[cfg(feature = "mod-suite")]
fn live_drop_item_name(item: i64) -> Option<String> {
    item_to_name(item)
        .map(str::to_string)
        .or_else(|| live_mod_registry_item_name(item))
}

#[cfg(feature = "mod-suite")]
fn live_stacks_to_loot_drops(drops: &[Stack]) -> Vec<(LootDropItem, Comps)> {
    drops
        .iter()
        .filter_map(|(item, count, comps)| {
            live_drop_item_name(*item)
                .map(|name| (LootDropItem::new(name, u32::from(*count)), comps.clone()))
        })
        .collect()
}

#[cfg(feature = "mod-suite")]
fn live_loot_drops_to_stacks(
    drops: Vec<LootDropItem>,
    original: &[(LootDropItem, Comps)],
) -> Vec<Stack> {
    drops
        .into_iter()
        .enumerate()
        .filter_map(|(index, drop)| {
            let item =
                name_to_item(&drop.item).or_else(|| live_mod_registry_item_id(&drop.item))?;
            let count = drop.count.clamp(1, u32::from(u8::MAX)) as u8;
            let comps = original
                .get(index)
                .filter(|(old, _)| old.item == drop.item)
                .map(|(_, comps)| comps.clone())
                .unwrap_or_default();
            Some((item, count, comps))
        })
        .collect()
}

#[cfg(feature = "mod-suite")]
fn live_loot_seed_with_context(
    mut seed: u64,
    dim: DimId,
    pos: Option<(i32, i32, i32)>,
    table: &str,
) -> u64 {
    for byte in table.as_bytes() {
        seed = (seed ^ u64::from(*byte)).wrapping_mul(0x1000_0000_01b3);
    }
    seed ^= u64::from(dim.raw()) << 48;
    if let Some((x, y, z)) = pos {
        seed ^= (x as i64 as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)
            ^ (y as i64 as u64).wrapping_mul(0xbf58_476d_1ce4_e5b9)
            ^ (z as i64 as u64).wrapping_mul(0x94d0_49bb_1331_11eb);
    }
    seed
}

#[cfg(feature = "mod-suite")]
fn apply_live_mod_loot_modifiers(
    loot_table_id: &str,
    dim: DimId,
    pos: Option<(i32, i32, i32)>,
    killer_is_player: bool,
    held_tool_name: Option<&str>,
    drops: Vec<Stack>,
    seed: u64,
) -> Vec<Stack> {
    let (mut engine, tables, item_tags) = {
        let suite = live_mod_suite().lock().unwrap_or_else(|e| e.into_inner());
        if !suite.active || suite.glm_engine.active_order().is_empty() {
            return drops;
        }
        (
            suite.glm_engine.clone(),
            suite.loot_tables_raw.clone(),
            suite.unified_tags.export_flat(TagDomain::Item),
        )
    };
    let seed = live_loot_seed_with_context(seed, dim, pos, loot_table_id);
    let original = live_stacks_to_loot_drops(&drops);
    let mut glm_drops: Vec<LootDropItem> = original.iter().map(|(drop, _)| drop.clone()).collect();
    let context = LootModifierContext {
        held_tool: held_tool_name.map(str::to_string),
        item_tags,
        killed_by_player: killer_is_player,
        enchanted_bonus_level: 0,
    };
    let resolver = |table: &str| {
        tables.get(table).map_or_else(Vec::new, |json| {
            live_stacks_to_loot_drops(&roll_loot_table_stacks(json, seed as i64, None))
                .into_iter()
                .map(|(drop, _)| drop)
                .collect()
        })
    };
    let report =
        engine.apply_loot_modifiers(loot_table_id, &context, &mut glm_drops, resolver, seed);
    if !report.fired_modifiers.is_empty() || !report.diagnostics.is_empty() {
        let mut suite = live_mod_suite().lock().unwrap_or_else(|e| e.into_inner());
        for modifier in &report.fired_modifiers {
            suite.loot_provenance.record_action(
                DiffDomain::LootTable,
                loot_table_id.to_string(),
                modifier.clone(),
                ProvenanceAction::ModifiedByGlm,
                format!("{modifier} modified {loot_table_id}"),
                None,
            );
        }
        suite.loot_diagnostics.extend(report.diagnostics.clone());
    }
    live_loot_drops_to_stacks(glm_drops, &original)
}

#[cfg(not(feature = "mod-suite"))]
#[inline(always)]
fn apply_live_mod_loot_modifiers(
    _loot_table_id: &str,
    _dim: DimId,
    _pos: Option<(i32, i32, i32)>,
    _killer_is_player: bool,
    _held_tool_name: Option<&str>,
    drops: Vec<Stack>,
    _seed: u64,
) -> Vec<Stack> {
    drops
}

#[cfg(feature = "mod-suite")]
fn live_json_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < ' ' => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(feature = "mod-suite")]
fn live_missing_dependency_plan_json(report: &MissingDependencyPlan) -> String {
    fn strings(values: &[String]) -> String {
        format!(
            "[{}]",
            values
                .iter()
                .map(|v| live_json_escape(v))
                .collect::<Vec<_>>()
                .join(",")
        )
    }
    let missing = report
        .missing_required
        .iter()
        .map(|m| {
            format!(
                "{{\"mod_id\":{},\"required_mod_id\":{},\"version_range\":{}}}",
                live_json_escape(&m.mod_id),
                live_json_escape(&m.required_mod_id),
                m.version_range
                    .as_deref()
                    .map(live_json_escape)
                    .unwrap_or_else(|| "null".into())
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{{\"admitted\":{},\"missing_required\":[{}],\"client_only_skipped\":{},\"modrinth_candidates\":{}}}",
        strings(&report.admitted),
        missing,
        strings(&report.client_only_skipped),
        report.modrinth_candidates.len()
    )
}

#[cfg(feature = "mod-suite")]
fn live_provenance_diff_json() -> String {
    let suite = live_mod_suite().lock().unwrap_or_else(|e| e.into_inner());
    let diff = suite.loot_reload_diff;
    let hooks = suite
        .bytecode_loot_hooks
        .iter()
        .map(|report| {
            format!(
                "{{\"mod_id\":{},\"hooks_detected\":{},\"requires_jvm_offload_for_loot\":{}}}",
                live_json_escape(&report.mod_id),
                report.hooks_detected.len(),
                report.requires_jvm_offload_for_loot
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let diagnostics = suite
        .loot_diagnostics
        .iter()
        .map(|line| live_json_escape(line))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{{\"schema\":\"megu-live-provenance-diff/1\",\"dependency_resolution\":{},\"reload_diff_summary\":{{\"added_recipes\":{},\"overridden_loot_tables\":{},\"active_glm_rules\":{},\"added_tags\":{},\"added_translations\":{},\"conflict_count\":{}}},\"active_glm_modifiers\":{},\"bytecode_loot_hooks\":[{}],\"loot_diagnostics\":[{}],\"provenance\":{}}}",
        live_missing_dependency_plan_json(&suite.dependency_resolution),
        diff.added_recipes,
        diff.overridden_loot_tables,
        diff.active_glm_rules,
        diff.added_tags,
        diff.added_translations,
        diff.conflict_count,
        suite.glm_engine.active_order().len(),
        hooks,
        diagnostics,
        suite.loot_provenance.to_json()
    )
}

#[cfg(not(feature = "mod-suite"))]
#[inline(always)]
fn live_provenance_diff_json() -> String {
    "{\"schema\":\"megu-live-provenance-diff/1\",\"records\":[],\"conflicts\":[]}".to_string()
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
            hook_fires: BTreeMap::new(),
            dual_loader: DualLoaderPlan::default(),
            damage_hooks: Vec::new(),
            culinary: CulinaryRegistry::new(),
            culinary_stats: CulinaryDiscoveryStats::default(),
            cooking_pots: HashMap::new(),
            unified_tags: UnifiedTagIndex::vanilla_1_21_1_baseline(),
            cooking_deadlines: std::collections::BTreeSet::new(),
            accessories: HashMap::new(),
            barriers: ProgressionBarrierEngine::new(Vec::new()),
            extra_advancements: BTreeSet::new(),
            appleskin: HashMap::new(),
            season_sync: HashMap::new(),
            mod_sync_body: None,
            mod_sync_root_hash: None,
            registry_plan: None,
            registry_revision: 0,
            mrs1_cache: None,
            recipe_conflicts: RecipeConflictIndex::new(),
            material_unifier: MaterialUnifier::new(
                mod_adapter_core::recipe_conflict_unify::MaterialUnifyConfig::default(),
            ),
            admitted_jars: BTreeSet::new(),
            admitted_mod_ids: BTreeSet::new(),
            dependency_resolution: MissingDependencyPlan::default(),
            loot_tables_raw: BTreeMap::new(),
            loot_overlay: ModLootOverlayIndex::new(),
            glm_engine: GlobalLootModifierEngine::new(),
            loot_provenance: ProvenanceDiffLedger::new(),
            previous_loot_provenance: ProvenanceDiffLedger::new(),
            loot_reload_diff: ReloadDiffSummary::default(),
            loot_diagnostics: Vec::new(),
            bytecode_loot_hooks: Vec::new(),
            addon_unsupported_serializers: BTreeSet::new(),
        }
    }

    /// One comparable signature token for an ingredient matcher — the same
    /// value a live lookup computes for its input slot, so registered and
    /// queried signatures meet: exact items are material-unified, tags keep
    /// their `#id` spelling, `AnyOf` reduces to its first alternative.
    fn signature_token(&self, matcher: &IngredientMatcher) -> String {
        match matcher {
            IngredientMatcher::Item(id) => {
                self.material_unifier.unify_item_id(id, &self.unified_tags)
            }
            IngredientMatcher::Tag(tag) => format!("#{tag}"),
            IngredientMatcher::AnyOf(alts) => alts
                .first()
                .map(|m| self.signature_token(m))
                .unwrap_or_default(),
        }
    }

    /// Rebuild the recipe conflict index over the merged culinary registry:
    /// two recipes claiming the same input signature become candidates the
    /// resolver ranks deterministically — exactly the OreDictionary overlap
    /// this surface exists for.
    fn rebuild_recipe_conflicts(&mut self) {
        self.recipe_conflicts = RecipeConflictIndex::new();
        for recipe in &self.culinary.cooking_recipes {
            let tokens: Vec<String> = recipe
                .ingredients
                .iter()
                .map(|matcher| self.signature_token(matcher))
                .collect();
            let signature = InputSignature::shapeless(tokens.iter().map(|token| token.as_str()));
            self.recipe_conflicts.register(
                signature,
                RecipeCandidate {
                    recipe_id: recipe.id.clone(),
                    output_item: recipe.result_item.clone(),
                    output_count: recipe.result_count,
                    mod_namespace: recipe
                        .id
                        .split(':')
                        .next()
                        .unwrap_or("minecraft")
                        .to_string(),
                },
            );
        }
        for recipe in &self.culinary.cutting_recipes {
            let signature = InputSignature::machine(
                "farmersdelight:cutting_board",
                &self.signature_token(&recipe.ingredient),
            );
            self.recipe_conflicts.register(
                signature,
                RecipeCandidate {
                    recipe_id: recipe.id.clone(),
                    output_item: recipe
                        .results
                        .first()
                        .map(|out| out.item.clone())
                        .unwrap_or_default(),
                    output_count: recipe.results.first().map(|out| out.count).unwrap_or(1),
                    mod_namespace: recipe
                        .id
                        .split(':')
                        .next()
                        .unwrap_or("minecraft")
                        .to_string(),
                },
            );
        }
    }

    /// Resolve one recipe lookup through the conflict index: when several
    /// registered candidates claim `signature`, the deterministic ranker
    /// picks the canonical one (vanilla first). `None` signature candidates
    /// fall through untouched — no conflicts registered yet.
    fn recipe_conflict_choice(&self, signature: String) -> Option<String> {
        self.recipe_conflicts
            .resolve(&signature, 0)
            .map(|choice| choice.selected.recipe_id.clone())
    }

    fn hook_bindings(
        &self,
        hook: StandardEventHook,
    ) -> impl Iterator<Item = &(String, LiftedBinding)> {
        let bindings = if self.active {
            self.lifted_hooks.as_slice()
        } else {
            &[]
        };
        bindings
            .iter()
            .filter(move |(_, binding)| binding.hook == hook)
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

    fn discover_loot_layers(
        &mut self,
        bounded_jars: &[(String, Vec<u8>)],
        adapted: &[LiveAdaptedJar],
    ) -> (Vec<LootDataLayer>, LiveLootSourceTexts, BTreeSet<String>) {
        let mut mod_layers = Vec::new();
        let mut raw_by_layer_path = LiveLootSourceTexts::new();
        let mut loot_ids = BTreeSet::new();
        for planned in &self.dual_loader.unified_mods {
            if !self.verified_jar(&planned.jar_name, adapted) {
                continue;
            }
            let Some((_, bytes)) = bounded_jars
                .iter()
                .find(|(name, _)| name == &planned.jar_name)
            else {
                continue;
            };
            let loot_entries = live_mod_zip_members(
                bytes,
                &|path| {
                    path.starts_with("data/")
                        && path.ends_with(".json")
                        && (path.contains("/loot_table/")
                            || path.contains("/loot_tables/")
                            || path.contains("/loot_modifier/")
                            || path.contains("/loot_modifiers/"))
                },
                4096,
                2 * 1024 * 1024,
                32 * 1024 * 1024,
            );
            match loot_entries {
                Ok(entries) => {
                    if !entries.is_empty() {
                        for entry in &entries {
                            if let Some(id) = live_loot_table_id_from_path(&entry.path) {
                                loot_ids.insert(id);
                            }
                            if let Ok(text) = String::from_utf8(entry.bytes.clone()) {
                                raw_by_layer_path
                                    .insert((planned.mod_id.clone(), entry.path.clone()), text);
                            }
                        }
                        mod_layers.push(LootDataLayer::new(
                            planned.mod_id.clone(),
                            LootLayerKind::Mod,
                            entries,
                        ));
                    }
                }
                Err(error) => self.loot_diagnostics.push(format!(
                    "{}: loot data scan rejected: {error}",
                    planned.jar_name
                )),
            }
            match live_mod_class_members(bytes) {
                Ok(classes) => {
                    let report = detect_bytecode_loot_hooks(&planned.mod_id, &classes);
                    if report.requires_jvm_offload_for_loot {
                        self.loot_diagnostics.push(format!(
                            "{}: Java bytecode loot hooks detected ({}); JSON loot overlay is incomplete and JVM offload is required for exact behavior",
                            planned.mod_id,
                            report.hooks_detected.len()
                        ));
                        self.loot_provenance.record_action(
                            DiffDomain::LootModifier,
                            format!("{}:bytecode_loot_hooks", planned.mod_id),
                            planned.mod_id.clone(),
                            ProvenanceAction::ModifiedByGlm,
                            format!(
                                "{} uses Java-only loot hooks; not reproduced by the declarative engine",
                                planned.mod_id
                            ),
                            None,
                        );
                        self.bytecode_loot_hooks.push(report);
                    }
                }
                Err(error) => self.loot_diagnostics.push(format!(
                    "{}: bytecode loot hook scan rejected: {error}",
                    planned.jar_name
                )),
            }
        }

        (mod_layers, raw_by_layer_path, loot_ids)
    }

    fn ingest_admitted_loot_layers(
        &mut self,
        bounded_jars: &[(String, Vec<u8>)],
        adapted: &[LiveAdaptedJar],
    ) {
        self.previous_loot_provenance = self.loot_provenance.clone();
        self.loot_provenance = ProvenanceDiffLedger::new();
        self.loot_tables_raw.clear();
        self.loot_diagnostics.clear();
        self.bytecode_loot_hooks.clear();
        let (mod_layers, raw_by_layer_path, loot_ids) =
            self.discover_loot_layers(bounded_jars, adapted);

        let vanilla_entries: Vec<_> = loot_ids
            .iter()
            .filter_map(|id| {
                let json = crate::chest_loot::vanilla_loot_table_json(id)?;
                let (ns, path) = id.split_once(':').unwrap_or(("minecraft", id));
                Some(LootDataEntry::new(
                    format!("data/{ns}/loot_table/{path}.json"),
                    json.as_bytes().to_vec(),
                ))
            })
            .collect();
        let mut overlay_layers = Vec::new();
        if !vanilla_entries.is_empty() {
            overlay_layers.push(LootDataLayer::new(
                "vanilla",
                LootLayerKind::Vanilla,
                vanilla_entries,
            ));
        }
        overlay_layers.extend(mod_layers.iter().cloned());

        match ModLootOverlayIndex::ingest_layers(&overlay_layers) {
            Ok(index) => {
                for record in index.provenance().records() {
                    self.loot_provenance.record(record.clone());
                }
                self.loot_diagnostics
                    .extend(index.diagnostics().iter().cloned());
                for (id, table) in index.tables() {
                    let key = (table.source_layer.clone(), table.path.clone());
                    if let Some(raw) = raw_by_layer_path.get(&key) {
                        self.loot_tables_raw.insert(id.clone(), raw.clone());
                    }
                }
                self.loot_overlay = index;
            }
            Err(error) => {
                self.loot_overlay = ModLootOverlayIndex::new();
                self.loot_diagnostics
                    .push(format!("loot table overlay rejected: {error}"));
            }
        }
        match GlobalLootModifierEngine::ingest_layers(&mod_layers) {
            Ok(engine) => {
                for record in engine.provenance().records() {
                    self.loot_provenance.record(record.clone());
                }
                self.loot_diagnostics
                    .extend(engine.diagnostics().iter().cloned());
                self.glm_engine = engine;
            }
            Err(error) => {
                self.glm_engine = GlobalLootModifierEngine::new();
                self.loot_diagnostics
                    .push(format!("global loot modifier ingest rejected: {error}"));
            }
        }
        for diagnostic in &self.loot_diagnostics {
            eprintln!("[live-mods] loot: {diagnostic}");
        }
        self.loot_reload_diff = self
            .loot_provenance
            .diff_summary_against(&self.previous_loot_provenance);
    }

    fn register_admitted_create_addons(
        &mut self,
        bounded_jars: &[(String, Vec<u8>)],
        adapted: &[LiveAdaptedJar],
    ) {
        // Both inputs feed only the `create-offload` addon pass below.
        #[cfg(not(feature = "create-offload"))]
        let _ = (bounded_jars, adapted);
        self.addon_unsupported_serializers.clear();
        #[cfg(feature = "create-offload")]
        for planned in &self.dual_loader.unified_mods {
            if planned.mod_id == "create" || !self.admitted_jars.contains(&planned.jar_name) {
                continue;
            }
            let depends_create = planned
                .dependencies
                .iter()
                .any(|dep| dep.mod_id == "create");
            let has_create_recipes = adapted.iter().any(|(name, result)| {
                name == &planned.jar_name
                    && result.as_ref().ok().is_some_and(|result| {
                        live_mod_result_verified(result)
                            && result
                                .data_bundle
                                .recipes
                                .iter()
                                .any(|recipe| recipe.recipe_type.starts_with("create:"))
                    })
            });
            if !depends_create && !has_create_recipes {
                continue;
            }
            let Some((_, bytes)) = bounded_jars
                .iter()
                .find(|(name, _)| name == &planned.jar_name)
            else {
                continue;
            };
            let before = aeronautics_bridge()
                .lock()
                .map(|bridge| bridge.unsupported_addon_serializers.clone())
                .unwrap_or_default();
            match aeronautics_bridge_discover_addon_jar([(
                planned.jar_name.as_str(),
                bytes.as_slice(),
            )]) {
                Ok(hidden) => {
                    if hidden != 0 {
                        eprintln!(
                            "[live-mods] create addon {}: hid {} ore/chest state ids",
                            planned.mod_id, hidden
                        );
                    }
                }
                Err(error) => eprintln!(
                    "[live-mods] create addon {} rejected: {error}",
                    planned.mod_id
                ),
            }
            let after = aeronautics_bridge()
                .lock()
                .map(|bridge| bridge.unsupported_addon_serializers.clone())
                .unwrap_or_default();
            for serializer in after.difference(&before) {
                eprintln!(
                    "[live-mods] create addon {}: unsupported recipe serializer {}",
                    planned.mod_id, serializer
                );
                self.addon_unsupported_serializers
                    .insert(serializer.clone());
            }
        }
    }

    fn read_bounded_jars(jars: &[(String, std::path::PathBuf)]) -> Vec<(String, Vec<u8>)> {
        let mut bounded_jars = Vec::new();
        let mut aggregate_bytes = 0u64;
        for (file_name, path) in jars {
            let Ok(metadata) = std::fs::metadata(path) else {
                eprintln!("[live-mods] {file_name}: cannot stat JAR — rejected");
                continue;
            };
            aggregate_bytes = aggregate_bytes.saturating_add(metadata.len());
            if metadata.len() > lmb_sync::MAX_MOD_JAR_BYTES {
                eprintln!("[live-mods] {file_name}: exceeds per-JAR byte budget — rejected");
                continue;
            }
            if aggregate_bytes > MAX_MODS_DIR_AGGREGATE_BYTES {
                eprintln!(
                    "[live-mods] {file_name}: aggregate mods/ input exceeds the {MAX_MODS_DIR_AGGREGATE_BYTES}-byte pack budget — rejected"
                );
                continue;
            }
            match std::fs::read(path) {
                Ok(bytes) => bounded_jars.push((file_name.clone(), bytes)),
                Err(error) => eprintln!("[live-mods] {file_name}: cannot read JAR: {error}"),
            }
        }

        bounded_jars
    }

    fn apply_dependency_admission(&mut self, plan: &DualLoaderPlan) {
        let (admitted_jars, admitted_mod_ids, admission_diagnostics) =
            strict_dependency_admission(plan);
        self.admitted_jars = admitted_jars;
        self.admitted_mod_ids = admitted_mod_ids;
        self.dependency_resolution = plan.resolution.clone();
        for line in &admission_diagnostics {
            eprintln!("[live-mods] admission: {line}");
        }
        eprintln!(
            "[live-mods] dependency admission: {} admitted, {} client-only, {} superseded, {} rejected, {} duplicate ids, {} missing deps",
            self.admitted_jars.len(),
            plan.skipped_client_only.len(),
            plan.skipped_superseded.len(),
            plan.rejected_jars.len(),
            plan.duplicate_conflicts.len(),
            plan.missing_dependencies.len(),
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
    }

    fn adapt_jars(
        dir: &std::path::Path,
        jars: &[(String, std::path::PathBuf)],
    ) -> Vec<LiveAdaptedJar> {
        let options = AutoAdaptOptions {
            target: LIVE_MOD_MC_TARGET,
            flagship_mod_ids: live_mod_flagship_ids(),
            ..AutoAdaptOptions::default()
        };
        // Adaptation is diagnostic until admission admits a JAR. The full-directory
        // pass is fail-closed, so a corrupt member retries per JAR exactly as before.
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
                    .map(|(name, path)| (name.clone(), auto_adapt_jar(path, &options)))
                    .collect()
            }
        }
    }

    fn verified_jar(&self, file_name: &str, adapted: &[LiveAdaptedJar]) -> bool {
        self.admitted_jars.contains(file_name)
            && adapted.iter().any(|(name, result)| {
                name == file_name && result.as_ref().ok().is_some_and(live_mod_result_verified)
            })
    }

    fn adopt_adapted_jars(&mut self, adapted: &[LiveAdaptedJar], plan: &DualLoaderPlan) {
        let skipped: BTreeSet<_> = plan
            .skipped_superseded
            .iter()
            .chain(&plan.skipped_client_only)
            .map(|jar| jar.jar_name.as_str())
            .collect();
        for (file_name, result) in adapted {
            let solver_admitted = self.admitted_jars.contains(file_name);
            match result {
                Ok(result) if solver_admitted && live_mod_result_verified(result) => {
                    self.adopt_jar_result(file_name, result)
                }
                Ok(result) => {
                    let reason = if result.tier == AdaptationTier::AutoSkipped
                        && (solver_admitted || skipped.contains(file_name.as_str()))
                    {
                        // Record the skip without adopting any content from the JAR.
                        result.audit_notes.join("; ")
                    } else if !solver_admitted {
                        "dependency admission rejected this JAR before activation".to_string()
                    } else {
                        format!(
                            "adaptation tier {} is not a verified native/Tier-2 activation: {}",
                            result.tier.as_str(),
                            result.blockers.join("; ")
                        )
                    };
                    eprintln!("[live-mods] {file_name}: {reason}");
                    self.reports
                        .push(LiveModJarReport::from_result(file_name, result, reason));
                }
                Err(error) => {
                    eprintln!("[live-mods] {file_name}: {error}");
                    let mod_id = live_mod_file_stem(file_name);
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
    }

    fn build_registry_plan(&mut self, dir: &std::path::Path, adapted: &[LiveAdaptedJar]) {
        let admitted_results: Vec<_> = adapted
            .iter()
            .filter_map(|(name, result)| {
                result
                    .as_ref()
                    .ok()
                    .filter(|result| {
                        self.admitted_jars.contains(name) && live_mod_result_verified(result)
                    })
                    .cloned()
            })
            .collect();
        if !admitted_results.is_empty() {
            let report = ModsDirAdaptationReport {
                dir: dir.display().to_string(),
                jars: admitted_results,
                non_jar_entries: 0,
                audit_notes: Vec::new(),
                dependency_resolution: Some(self.dependency_resolution.clone()),
                dependency_recovery: Vec::new(),
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
        } else {
            self.registry_plan = None;
        }
    }

    fn load_admitted_jar_data(
        &mut self,
        bounded_jars: &[(String, Vec<u8>)],
        adapted: &[LiveAdaptedJar],
        plan: &DualLoaderPlan,
    ) {
        let culinary_live: BTreeSet<_> = adapted
            .iter()
            .filter_map(|(name, result)| {
                result
                    .as_ref()
                    .ok()
                    .filter(|r| r.culinary.is_some())
                    .map(|_| name.as_str())
            })
            .collect();
        for (file_name, bytes) in bounded_jars {
            if !self.verified_jar(file_name, adapted) {
                continue;
            }
            if culinary_live.contains(file_name.as_str()) {
                if let Err(error) = self.culinary.load_from_jar_bytes(bytes) {
                    eprintln!("[live-mods] {file_name}: culinary scan skipped: {error:?}");
                }
            }
            if let Err(error) = self.unified_tags.ingest_jar_bytes(bytes, file_name) {
                eprintln!("[live-mods] {file_name}: unified tag scan skipped: {error}");
            }
            if plan
                .unified_mods
                .iter()
                .any(|entry| entry.jar_name == *file_name)
            {
                self.load_damage_hooks(file_name, bytes);
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
    }

    /// Solve dependency admission before publishing any JAR content.
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
        let bounded_jars = Self::read_bounded_jars(&jars);
        let plan = plan_dual_loader_mods_dir(
            bounded_jars
                .iter()
                .map(|(name, bytes)| (name.as_str(), bytes.as_slice())),
        );
        self.apply_dependency_admission(&plan);
        let adapted = Self::adapt_jars(dir, &jars);
        self.adopt_adapted_jars(&adapted, &plan);
        self.build_registry_plan(dir, &adapted);
        self.load_admitted_jar_data(&bounded_jars, &adapted, &plan);
        self.dual_loader = plan;
        self.ingest_admitted_loot_layers(&bounded_jars, &adapted);
        self.register_admitted_create_addons(&bounded_jars, &adapted);
        publish_live_mod_loot_underlay(&self.loot_tables_raw);

        for domain in [TagDomain::Item, TagDomain::Block] {
            for (tag_id, members) in self.unified_tags.export_flat(domain) {
                self.culinary
                    .tags
                    .entry(tag_id)
                    .or_default()
                    .extend(members);
            }
        }
        self.registry_revision = self.registry_revision.wrapping_add(1);
        self.mrs1_cache = None;
        self.rebuild_recipe_conflicts();
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
    /// Called only for solver-admitted, verified native/Tier-2 JARs.
    fn adopt_jar_result(&mut self, file_name: &str, result: &JarAdaptationResult) {
        if let Some(bundle) = &result.bundle {
            self.install_bundle(bundle, file_name);
        }
        // XMOD1: fold this JAR's extracted `data/<ns>/tags/**` table into the
        // shared index — equivalent to merging `result.data_bundle
        // .unified_tag_index()`; `data_bundle.tags` already carries
        // Jar-in-Jar inner-JAR tag files folded in by the extractor.
        self.unified_tags
            .merge_auto_adapter_tags(&result.data_bundle.tags);
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
        let diagnostic = if result.blockers.is_empty() {
            result.audit_notes.join("; ")
        } else {
            result.blockers.join("; ")
        };
        let report = LiveModJarReport::from_result(file_name, result, diagnostic);
        let mod_id = &report.mod_id;
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
        self.reports.push(report);
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
    fn set_cooking_pot(&mut self, key: (DimId, i32, i32, i32), pot: CookingPotState, tick: u64) {
        if let Some(previous) = self.cooking_pots.get(&key) {
            if let Some(deadline) = previous
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pot()
                .next_completion_tick
            {
                self.cooking_deadlines
                    .remove(&(deadline, key.0, key.1, key.2, key.3));
            }
        }
        let adapter = self
            .cooking_pots
            .entry(key)
            .or_insert_with(|| Arc::new(Mutex::new(CookingPotItemAdapter::new())))
            .clone();
        {
            let mut guard = adapter.lock().unwrap_or_else(|e| e.into_inner());
            *guard.pot_mut() = pot;
            guard.pot_mut().refresh(&self.culinary, tick);
            if let Some(deadline) = guard.pot().next_completion_tick {
                self.cooking_deadlines
                    .insert((deadline, key.0, key.1, key.2, key.3));
            }
        }
        // XMOD1: the canonical adapter doubles as this cell's live bus
        // endpoint — items a Create duct or foreign mod pushes through
        // `live_cap_insert_item`/`live_cap_bus_step` land in the same pot
        // the scheduler just armed.
        live_cap_bus_guard().register_item_handler(
            live_cap_coord(key.0, key.1, key.2, key.3),
            Box::new(LiveSharedItemEndpoint { inner: adapter }),
        );
    }

    /// Re-arm `key`'s completion deadline after its slots changed outside
    /// [`Self::set_cooking_pot`] — the bus-faced path
    /// (`live_cap_notify_pot_fed`) funnels here so an ingredient a duct
    /// pushed in wakes a sleeping pot exactly like a manual insert.
    fn refresh_cooking_pot(&mut self, key: (DimId, i32, i32, i32), tick: u64) {
        let Some(adapter) = self.cooking_pots.get(&key).cloned() else {
            return;
        };
        let mut guard = adapter.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(old) = guard.pot().next_completion_tick {
            self.cooking_deadlines
                .remove(&(old, key.0, key.1, key.2, key.3));
        }
        guard.pot_mut().refresh(&self.culinary, tick);
        if let Some(deadline) = guard.pot().next_completion_tick {
            self.cooking_deadlines
                .insert((deadline, key.0, key.1, key.2, key.3));
        }
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
            let Some(adapter) = self.cooking_pots.get(&key).cloned() else {
                continue;
            };
            let mut guard = adapter.lock().unwrap_or_else(|e| e.into_inner());
            if guard.pot().next_completion_tick != Some(deadline) {
                continue;
            }
            if let Some(result) = guard.advance_to(tick, &self.culinary) {
                completed.push((key, result));
            }
            if let Some(next) = guard.pot().next_completion_tick {
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
        for summary in &self.auto_adapted {
            eprintln!(
                "[live-mods] adapted {} ({}) tier {:?}: {} lifted hook(s), {} cooking + {} cutting recipe(s), {} tag file(s) / {} tag value(s)",
                summary.file_name,
                summary.mod_id,
                summary.tier,
                summary.lifted_hooks.len(),
                summary.cooking_recipes,
                summary.cutting_recipes,
                summary.tag_files,
                summary.tag_values,
            );
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
    const MAX_CLASSES: usize = 8192;
    const MAX_CLASS_BYTES: usize = 2 * 1024 * 1024;
    const MAX_TOTAL_CLASS_BYTES: usize = 64 * 1024 * 1024;

    let (mut cursor, end, count) = live_zip_directory(jar)?;
    let mut decoded_total = 0;
    let mut classes = Vec::new();
    let kind = LiveZipKind::Class;
    for _ in 0..count {
        let member = LiveZipMember::read(jar, &mut cursor, end, kind)?;
        if !member.name.ends_with(b".class") {
            continue;
        }
        member.check_bounds(classes.len(), MAX_CLASSES, MAX_CLASS_BYTES, kind)?;
        let name = std::str::from_utf8(member.name).map_err(|_| "non-UTF8 class name")?;
        let decoded = member.decode(jar, name, MAX_CLASS_BYTES, kind)?;
        member.account_decoded(
            decoded.len(),
            &mut decoded_total,
            MAX_TOTAL_CLASS_BYTES,
            kind,
        )?;
        if megu_core::hash::crc32_ieee(&decoded) != member.crc {
            return Err("class crc mismatch");
        }
        classes.push((name.to_string(), decoded));
    }
    if cursor != end {
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

/// Lock-free "a mod event hook might fire" gate consulted by every live
/// dispatch site: vanilla and zero-mod servers pay exactly one relaxed
/// atomic load. Refreshed whenever the process-wide suite is (re)installed
/// — `live_mod_suite_boot` in production, `fresh_suite`/suite installs in
/// tests — so a stale `true` can only cost one suite-lock re-check, never a
/// missed hook.
#[cfg(feature = "mod-suite")]
static HAS_LIVE_MOD_HOOKS: AtomicBool = AtomicBool::new(false);

/// Whether the armed suite can produce any event-hook work: lifted Mixin
/// bindings or verified `onDamage` handlers.
#[cfg(feature = "mod-suite")]
fn live_mod_suite_has_hooks(suite: &LiveModSuite) -> bool {
    suite.active && (!suite.lifted_hooks.is_empty() || !suite.damage_hooks.is_empty())
}

/// Recompute [`HAS_LIVE_MOD_HOOKS`] from the installed suite.
#[cfg(feature = "mod-suite")]
fn live_mod_refresh_hook_gate() {
    let armed = {
        let suite = live_mod_suite().lock().unwrap_or_else(|e| e.into_inner());
        live_mod_suite_has_hooks(&suite)
    };
    HAS_LIVE_MOD_HOOKS.store(armed, Ordering::Relaxed);
}

/// Snapshot of the automatically registered native mod bundles.
#[cfg(feature = "mod-suite")]
pub fn live_mod_unified_runtime() -> UnifiedModRuntime {
    live_mod_suite()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .unified
        .clone()
}

/// Snapshot of the active subscriptions in boot-scan order, for tests.
#[cfg(all(test, feature = "mod-suite"))]
fn live_mod_dispatch_hook(hook: StandardEventHook) -> Vec<(String, LiftedBinding)> {
    live_mod_suite()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .hook_bindings(hook)
        .cloned()
        .collect()
}

/// The only capabilities exposed to verified damage bytecode. Each method
/// receives a fresh host so that failures cannot leak partial mutations.
#[cfg(feature = "mod-suite")]
struct LiveModDamageHost {
    damage: f32,
    cancelled: bool,
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
            (HostCallKind::EventCancel, [VmValue::Ref(1)]) => {
                self.cancelled = true;
                Ok(None)
            }
            (HostCallKind::EventIsCanceled, [VmValue::Ref(1)]) => {
                Ok(Some(VmValue::Int(i32::from(self.cancelled))))
            }
            (HostCallKind::EventSetCanceled, [VmValue::Ref(1), VmValue::Int(value)]) => {
                self.cancelled = *value != 0;
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
/// Returns the (possibly rewritten) damage plus the sticky cancel flag —
/// `event.cancel()`/`event.setCanceled(true)` in any handler cancels the
/// whole hit.
#[cfg(feature = "mod-suite")]
fn live_mod_eval_damage_event(initial_damage: f32) -> (f32, bool) {
    if !initial_damage.is_finite() || initial_damage < 0.0 {
        return (initial_damage, false);
    }
    let suite = live_mod_suite().lock().unwrap_or_else(|e| e.into_inner());
    if !suite.active || suite.damage_hooks.is_empty() {
        return (initial_damage, false);
    }
    let mut damage = initial_damage;
    let mut cancelled = false;
    for method in &suite.damage_hooks {
        let mut host = LiveModDamageHost { damage, cancelled };
        match execute_verified_method(method, &[VmValue::Ref(1)], &mut host, DEFAULT_FUEL) {
            Ok(_) => {
                damage = host.damage;
                cancelled |= host.cancelled;
            }
            Err(error) => eprintln!("[live-mods] damage hook execution rejected: {error}"),
        }
    }
    (damage, cancelled)
}

/// Damage-only view of [`live_mod_eval_damage_event`] kept for the existing
/// callers/tests: the rewritten amount, cancel flag dropped.
#[cfg(feature = "mod-suite")]
pub fn live_mod_eval_damage_hooks(initial_damage: f32) -> f32 {
    live_mod_eval_damage_event(initial_damage).0
}

/// Record a dispatch when a lifted binding subscribes to `hook`. Returns
/// whether the fast gate was armed, even without lifted bindings (verified
/// damage bytecode can be armed independently).
#[cfg(feature = "mod-suite")]
fn live_mod_fire_hook(hook: StandardEventHook) -> bool {
    if !HAS_LIVE_MOD_HOOKS.load(Ordering::Relaxed) {
        return false;
    }
    let subscribed = live_mod_suite()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .hook_bindings(hook)
        .next()
        .is_some();
    if subscribed {
        let mut suite = live_mod_suite().lock().unwrap_or_else(|e| e.into_inner());
        *suite.hook_fires.entry(hook).or_default() += 1;
    }
    true
}

/// Pre-damage dispatch wired into the real damage-application paths in
/// `combat_and_health.rs` (the `Entity.hurt` HEAD equivalent): fires the
/// lifted `EntityDamagePre` bindings, then runs the verified `onDamage`
/// handlers which may rewrite the amount (`event.setDamage`) or cancel the
/// hit (`event.cancel`). `Some(amount)` applies `amount`; `None` cancels —
/// vanilla and zero-mod servers pay exactly one relaxed atomic load.
#[cfg(feature = "mod-suite")]
fn live_mod_entity_damage_pre(amount: f32) -> Option<f32> {
    if !live_mod_fire_hook(StandardEventHook::EntityDamagePre) {
        return Some(amount);
    }
    let (damage, cancelled) = live_mod_eval_damage_event(amount);
    (!cancelled).then_some(damage)
}

/// Install an updated pot snapshot after an inventory, heat, or block event.
/// `dim` is the raw dimension slot ([`DimId::raw`]).
#[cfg(feature = "mod-suite")]
pub fn live_mod_set_cooking_pot(dim: u32, x: i32, y: i32, z: i32, pot: CookingPotState, tick: u64) {
    let dim = DimId::from_raw(dim);
    let mut suite = live_mod_suite().lock().unwrap_or_else(|e| e.into_inner());
    if suite.active {
        suite.set_cooking_pot((dim, x, y, z), pot, tick);
    }
}

/// Remove a broken pot and its scheduled wake-up, if any.
/// `dim` is the raw dimension slot ([`DimId::raw`]).
#[cfg(feature = "mod-suite")]
pub fn live_mod_remove_cooking_pot(dim: u32, x: i32, y: i32, z: i32) {
    let dim = DimId::from_raw(dim);
    let mut suite = live_mod_suite().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(adapter) = suite.cooking_pots.remove(&(dim, x, y, z)) {
        if let Some(deadline) = adapter
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pot()
            .next_completion_tick
        {
            suite.cooking_deadlines.remove(&(deadline, dim, x, y, z));
        }
        // The pot's shared-bus endpoint dies with it — a Create duct can
        // never feed a phantom inventory.
        live_cap_bus_guard().unregister_item_handler(&live_cap_coord(dim, x, y, z));
    }
}

/// Wake only pots whose deadline is due. Empty or unheated pots perform no
/// per-tick work, irrespective of how many are registered.
#[cfg(feature = "mod-suite")]
fn live_mod_tick_cooking_pots(tick: u64) -> Vec<((DimId, i32, i32, i32), CookResult)> {
    let mut suite = live_mod_suite().lock().unwrap_or_else(|e| e.into_inner());
    if suite.active {
        suite.tick_cooking_pots(tick)
    } else {
        Vec::new()
    }
}

/// Execute cutting recipes discovered from base and unknown-addon JARs.
/// The live lookup surface runs the same arbitration `scan_mods_dir` set
/// up: the input is material-unified, the recipe conflict index arbitrates
/// overlaps deterministically, and only the winner's recipe executes when
/// it truly claims this input+tool — an addon can never shadow a canonical
/// recipe by scan order alone.
#[cfg(feature = "mod-suite")]
fn live_mod_cutting_board_use(item: &str, tool: &str, seed: u64) -> Option<CuttingOutcome> {
    let suite = live_mod_suite().lock().unwrap_or_else(|e| e.into_inner());
    if !suite.active {
        return None;
    }
    let canonical = suite
        .material_unifier
        .unify_item_id(item, &suite.unified_tags);
    let signature = InputSignature::machine("farmersdelight:cutting_board", &canonical);
    if let Some(winner) = suite.recipe_conflict_choice(signature) {
        if let Some(recipe) = suite
            .culinary
            .cutting_recipes
            .iter()
            .find(|recipe| recipe.id == winner)
        {
            if !recipe.ingredient.matches(&canonical, &suite.culinary.tags)
                || !recipe.tool.matches(tool, &suite.culinary.tags)
            {
                return None;
            }
            // Arbitration winner claims this slot: roll its weighted
            // outputs exactly once — the same rule `execute_cutting`
            // applies to its first match.
            let mut rng = megu_core::random::Xoroshiro128pp::new(seed);
            let outputs: Vec<(String, u16)> = recipe
                .results
                .iter()
                .filter(|out| out.chance >= 1.0 || rng.next_float() < out.chance)
                .map(|out| (out.item.clone(), out.count))
                .collect();
            if outputs.is_empty() {
                return None;
            }
            return Some(CuttingOutcome {
                recipe_id: recipe.id.clone(),
                outputs,
                sound: recipe.sound.clone(),
            });
        }
    }
    suite.culinary.execute_cutting(&canonical, tool, seed)
}

// ---------------------------------------------------------------------------
// XMOD1: one shared cross-mod capability bus + one shared tag index.
// ---------------------------------------------------------------------------

/// One shared `#c:*`/`#forge:*`/`#neoforge:*` membership probe over the
/// merged index every scanned mod JAR (and Jar-in-Jar member) fed during
/// `scan_mods_dir`, seeded with the Vanilla 1.21.1 `c:` baseline. Nested
/// references expand at query time under the index's bounded-depth,
/// fail-closed rules; `tag` accepts the bare path or an already-prefixed
/// `#tag`/`ns:path` spelling the index canonicalizes.
#[cfg(feature = "mod-suite")]
fn live_unified_tag_matches(domain: TagDomain, tag: &str, id: &str) -> bool {
    live_mod_suite()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .unified_tags
        .matches(domain, tag, id)
}

/// The process-wide cross-mod capability bus: Create ducts and tanks,
/// culinary cooking pots, generic storage endpoints, and every
/// `ModOffloadProfile`-declared mod block register their
/// item/fluid/energy handlers here so transfers between mods need zero
/// per-mod glue. Poison-tolerant like every live bridge singleton.
#[cfg(feature = "mod-suite")]
fn live_cap_bus() -> &'static Mutex<BlockCapabilityBus> {
    static BUS: OnceLock<Mutex<BlockCapabilityBus>> = OnceLock::new();
    BUS.get_or_init(|| Mutex::new(BlockCapabilityBus::new()))
}

#[cfg(feature = "mod-suite")]
fn live_cap_bus_guard() -> std::sync::MutexGuard<'static, BlockCapabilityBus> {
    live_cap_bus().lock().unwrap_or_else(|e| e.into_inner())
}

/// Endpoint-registration bookkeeping kept beside the bus: which cells
/// currently expose a Create or declarative-profile handler, so block
/// removal unregisters the right thing and plain state-id churn
/// (facing/powered flips) never silently drops stored cargo.
#[cfg(feature = "mod-suite")]
#[derive(Default)]
struct LiveCapState {
    /// `FactoryPos -> block_id` for every Create cell with a live bus
    /// endpoint.
    create_cells: BTreeMap<FactoryPos, String>,
    /// `BlockCoord`s registered from
    /// `ModOffloadProfile::capability_endpoints_for`.
    profile_cells: BTreeSet<BlockCoord>,
    /// `(dim, x, y, z) -> shape` for every JAR-declared block whose id
    /// implied a generic capability endpoint — the bookkeeping that lets
    /// `live_cap_sync_generic_endpoint` drop exactly the kind it
    /// registered and lets `live_cap_generic_step` find the cells.
    generic_cells: BTreeMap<(DimId, i32, i32, i32), GenericCapShape>,
}

#[cfg(feature = "mod-suite")]
fn live_cap_state() -> &'static Mutex<LiveCapState> {
    static STATE: OnceLock<Mutex<LiveCapState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(LiveCapState::default()))
}

/// Canonical `BlockCoord` for a world cell: vanilla dims keep their
/// `"minecraft:*"` ids, registered custom dims their datapack key, and
/// unregistered raw slots still get a stable unique id.
#[cfg(feature = "mod-suite")]
fn live_cap_coord(dim: DimId, x: i32, y: i32, z: i32) -> BlockCoord {
    let id = dim.dimension_id();
    BlockCoord::new(
        if id.is_empty() {
            format!("megu:dim_{}", dim.raw())
        } else {
            id.to_string()
        },
        x,
        y,
        z,
    )
}

#[cfg(feature = "mod-suite")]
fn live_cap_coord_of(pos: FactoryPos) -> BlockCoord {
    live_cap_coord(DimId::from_raw(pos.dim as u32), pos.x, pos.y, pos.z)
}

/// `(dx,dy,dz)` -> the world face it points through.
#[cfg(feature = "mod-suite")]
fn live_cap_face_of(d: (i32, i32, i32)) -> FaceDirection {
    match d {
        (0, -1, 0) => FaceDirection::Down,
        (0, 1, 0) => FaceDirection::Up,
        (0, 0, -1) => FaceDirection::North,
        (0, 0, 1) => FaceDirection::South,
        (-1, 0, 0) => FaceDirection::West,
        _ => FaceDirection::East,
    }
}

/// Internal `i64` item id → the namespaced bus id. `item_to_name` yields
/// bare vanilla paths ("wheat"); the bus speaks namespaced ids, so
/// unqualified names re-qualify as `minecraft:*` while `create:*` and
/// other already-namespaced names pass through untouched. Unknown ids
/// get a stable `megu:item_*` placeholder so cargo never loses identity.
#[cfg(feature = "mod-suite")]
fn live_cap_item_name(item: i64) -> String {
    item_to_name(item)
        .map(live_namespaced_item_name)
        .unwrap_or_else(|| format!("megu:item_{item}"))
}

/// Items a single `live_cap_bus_step` lets one Create duct push into a
/// foreign endpoint — one partial stack per tick, matching the staging
/// cadence the transport layer uses.
#[cfg(feature = "mod-suite")]
const LIVE_CAP_STEP_ITEMS: u32 = 16;

/// Fluid (mB) a `live_cap_bus_step` lets one Create pipe/pump push toward
/// each adjacent fluid endpoint — one bucket per tick per face.
#[cfg(feature = "mod-suite")]
const LIVE_CAP_STEP_FLUID_MB: u64 = 1_000;

/// A bus endpoint backed by the same runtime the pot or forge scheduler
/// owns, so inserts/extracts mutate canonical state (not a shadow inventory).
#[cfg(feature = "mod-suite")]
struct LiveSharedItemEndpoint<T> {
    inner: Arc<Mutex<T>>,
}

#[cfg(feature = "mod-suite")]
impl<T: ItemTransferHandler + 'static> ItemTransferHandler for LiveSharedItemEndpoint<T> {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn insert_item(
        &mut self,
        face: Option<FaceDirection>,
        stack: &CapabilityItemStack,
        simulate: bool,
    ) -> CapabilityItemStack {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert_item(face, stack, simulate)
    }

    fn extract_item(
        &mut self,
        face: Option<FaceDirection>,
        max_count: u32,
        filter: Option<&dyn Fn(&CapabilityItemStack) -> bool>,
        simulate: bool,
    ) -> Option<CapabilityItemStack> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extract_item(face, max_count, filter, simulate)
    }

    fn slots(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).slots()
    }
}

/// Insert `count` of `item_id` (with lossless 1.21.1 `raw_components`)
/// into the capability endpoint at `(dim, x, y, z)` through `face`
/// (`None` = internal access). Returns the accepted count; `simulate`
/// probes without mutating. `0` when no endpoint occupies the cell.
///
/// A live Create duct cell takes component-free stacks with a resolvable
/// live item id straight through [`live_create_duct_insert`] — the
/// transport FIFO for moving ducts, the `held` slot for depots/basins —
/// the same hopper-insert path vanilla-adjacent consumers use. That call
/// runs BEFORE the bus lock: it takes `live_create_state` internally and
/// the bus never nests inside it. Component-tagged stacks and foreign
/// item ids skip the duct and land in the endpoint's slot store where
/// their bytes and ids survive losslessly.
#[cfg(feature = "mod-suite")]
#[allow(clippy::too_many_arguments)]
fn live_cap_insert_item(
    dim: DimId,
    x: i32,
    y: i32,
    z: i32,
    face: Option<FaceDirection>,
    item_id: &str,
    count: u32,
    raw_components: &[u8],
    simulate: bool,
) -> u32 {
    let coord = live_cap_coord(dim, x, y, z);
    let mut stack = CapabilityItemStack::with_components(item_id, count, raw_components.to_vec());
    let pos = FactoryPos::new(dim.raw() as i32, x, y, z);
    if stack.raw_components.is_empty() {
        let is_duct = {
            let state = live_cap_state().lock().unwrap_or_else(|e| e.into_inner());
            state
                .create_cells
                .get(&pos)
                .is_some_and(|id| live_duct_kind(id).is_some())
        };
        if is_duct {
            if let Some(item) = name_to_item(item_id) {
                if simulate {
                    // Read-only probe of the live duct's room: `held` slot
                    // for depot/basin, staging-FIFO headroom elsewhere.
                    let room = live_create_with(|rt| {
                        let kind = rt.duct_kind_at(pos)?;
                        Some(match kind {
                            LiveDuctKind::Depot | LiveDuctKind::Basin => kind
                                .held_capacity()
                                .saturating_sub(rt.held.get(&pos).map_or(0, |h| h.items.len()))
                                as u32,
                            _ => {
                                let used = rt
                                    .duct_machines
                                    .get(&pos)
                                    .and_then(|_| rt.duct_machine_state(pos))
                                    .map(|m| m.input_items)
                                    .unwrap_or(0);
                                kind.capacity().saturating_sub(used as u32)
                            }
                        })
                    })
                    .flatten()
                    .unwrap_or(0);
                    stack.count = stack.count.saturating_sub(room);
                } else {
                    while stack.count > 0 && live_create_duct_insert(dim, x, y, z, item) {
                        stack.count -= 1;
                    }
                }
            }
        }
    }
    if stack.count == 0 {
        return count;
    }
    let refused = {
        let mut bus = live_cap_bus_guard();
        bus.item_endpoint_mut(&coord)
            .map_or(stack.count, |endpoint| {
                endpoint.insert_item(face, &stack, simulate).count
            })
    };
    let accepted = count.saturating_sub(refused);
    if accepted > 0 && !simulate {
        // A fed cooking pot may have become cookable — re-arm its
        // deadline outside the bus lock.
        live_cap_notify_pot_fed(&coord);
    }
    accepted
}

/// Extract up to `max_count` items from the endpoint at
/// `(dim, x, y, z)` through `face`; the returned stack keeps its
/// `raw_components` byte-identical. `None` when the cell hosts no
/// endpoint or holds nothing extractable. Depot/basin cells additionally
/// serve their `held` slot — hopper-style pulls read it here; cargo
/// inside the transport machinery itself is not pullable, matching real
/// Create.
#[cfg(feature = "mod-suite")]
fn live_cap_extract_item(
    dim: DimId,
    x: i32,
    y: i32,
    z: i32,
    face: Option<FaceDirection>,
    max_count: u32,
    simulate: bool,
) -> Option<CapabilityItemStack> {
    let coord = live_cap_coord(dim, x, y, z);
    {
        let mut bus = live_cap_bus_guard();
        if let out @ Some(_) = bus
            .item_endpoint_mut(&coord)
            .and_then(|ep| ep.extract_item(face, max_count, None, simulate))
        {
            return out;
        }
    }
    let pos = FactoryPos::new(dim.raw() as i32, x, y, z);
    let held_duct = {
        let state = live_cap_state().lock().unwrap_or_else(|e| e.into_inner());
        state.create_cells.get(&pos).is_some_and(|id| {
            matches!(
                live_duct_kind(id),
                Some(LiveDuctKind::Depot | LiveDuctKind::Basin)
            )
        })
    };
    if !held_duct {
        return None;
    }
    live_create_with(|rt| {
        let &item = rt.held.get(&pos)?.items.last()?;
        let name = live_cap_item_name(item);
        if !simulate {
            let h = rt.held.get_mut(&pos)?;
            h.items.pop();
            if h.items.is_empty() {
                rt.held.remove(&pos);
            }
            // AUTO-OPT: a pull changed the machine's inputs — reset its
            // cadence so the recipe fingerprint re-evaluates at once.
            live_auto_machine_wake_positions(&[pos]);
        }
        Some(CapabilityItemStack::new(name, 1))
    })
    .flatten()
}

/// Atomically move up to `max_count` items between the endpoints at
/// `from`/`to` inside `dim` — the bus's simulate-then-commit contract
/// makes every hop all-or-nothing, so a full destination can never
/// duplicate or vanish cargo.
#[cfg(feature = "mod-suite")]
fn live_cap_transfer_adjacent(
    dim: DimId,
    from: [i32; 3],
    from_face: FaceDirection,
    to: [i32; 3],
    to_face: FaceDirection,
    max_count: u32,
) -> u32 {
    live_cap_bus_guard().transfer_item_between(
        &live_cap_coord(dim, from[0], from[1], from[2]),
        Some(from_face),
        &live_cap_coord(dim, to[0], to[1], to[2]),
        Some(to_face),
        max_count,
        None,
    )
}

/// Atomically move up to `max_mb` of fluid between two bus endpoints.
#[cfg(feature = "mod-suite")]
fn live_cap_transfer_fluid(
    dim: DimId,
    from: [i32; 3],
    from_face: FaceDirection,
    to: [i32; 3],
    to_face: FaceDirection,
    max_mb: u64,
) -> u64 {
    live_cap_bus_guard().transfer_fluid_between(
        &live_cap_coord(dim, from[0], from[1], from[2]),
        Some(from_face),
        &live_cap_coord(dim, to[0], to[1], to[2]),
        Some(to_face),
        max_mb,
    )
}

/// Atomically move up to `max_fe` of FE energy between two bus endpoints.
#[cfg(feature = "mod-suite")]
fn live_cap_transfer_energy(
    dim: DimId,
    from: [i32; 3],
    from_face: FaceDirection,
    to: [i32; 3],
    to_face: FaceDirection,
    max_fe: u64,
) -> u64 {
    live_cap_bus_guard().transfer_energy_between(
        &live_cap_coord(dim, from[0], from[1], from[2]),
        Some(from_face),
        &live_cap_coord(dim, to[0], to[1], to[2]),
        Some(to_face),
        max_fe,
    )
}

/// Fill the fluid endpoint at `(dim, x, y, z)` with `fluid_id`
/// (component bytes preserved); returns accepted mB.
#[cfg(feature = "mod-suite")]
#[allow(clippy::too_many_arguments)]
fn live_cap_fill_fluid(
    dim: DimId,
    x: i32,
    y: i32,
    z: i32,
    fluid_id: &str,
    amount_mb: u64,
    raw_components: &[u8],
    simulate: bool,
) -> u64 {
    let stack = CapabilityFluidStack::with_components(fluid_id, amount_mb, raw_components.to_vec());
    live_cap_bus_guard()
        .fluid_endpoint_mut(&live_cap_coord(dim, x, y, z))
        .map_or(0, |ep| ep.fill_fluid(None, &stack, simulate))
}

/// Drain up to `max_mb` from the fluid endpoint at `(dim, x, y, z)`.
#[cfg(feature = "mod-suite")]
fn live_cap_drain_fluid(
    dim: DimId,
    x: i32,
    y: i32,
    z: i32,
    max_mb: u64,
    simulate: bool,
) -> Option<CapabilityFluidStack> {
    live_cap_bus_guard()
        .fluid_endpoint_mut(&live_cap_coord(dim, x, y, z))?
        .drain_fluid(None, max_mb, None, simulate)
}

/// Push `max_fe` into the energy endpoint at `(dim, x, y, z)`; returns
/// accepted FE.
#[cfg(feature = "mod-suite")]
fn live_cap_receive_energy(dim: DimId, x: i32, y: i32, z: i32, max_fe: u64, simulate: bool) -> u64 {
    live_cap_bus_guard()
        .energy_endpoint_mut(&live_cap_coord(dim, x, y, z))
        .map_or(0, |ep| ep.receive_energy(None, max_fe, simulate))
}

/// Register a generic `slots`-deep item endpoint (hopper/chest-shaped
/// storage — the shape vanilla containers and data-driven mod blocks
/// share) at `(dim, x, y, z)`.
#[cfg(feature = "mod-suite")]
fn live_cap_register_slot_inventory(
    dim: DimId,
    x: i32,
    y: i32,
    z: i32,
    slots: usize,
    max_stack: u32,
) {
    live_cap_bus_guard().register_item_handler(
        live_cap_coord(dim, x, y, z),
        Box::new(SimpleSlotInventory::new(slots, max_stack)),
    );
}

/// Register a generic `capacity_mb` fluid endpoint at `(dim, x, y, z)`.
#[cfg(feature = "mod-suite")]
fn live_cap_register_fluid_tank(dim: DimId, x: i32, y: i32, z: i32, capacity_mb: u64) {
    live_cap_bus_guard().register_fluid_handler(
        live_cap_coord(dim, x, y, z),
        Box::new(SimpleFluidTank::new(capacity_mb)),
    );
}

/// Register a generic `capacity_fe` FE buffer at `(dim, x, y, z)`.
#[cfg(feature = "mod-suite")]
fn live_cap_register_energy_buffer(dim: DimId, x: i32, y: i32, z: i32, capacity_fe: u64) {
    live_cap_bus_guard().register_energy_handler(
        live_cap_coord(dim, x, y, z),
        Box::new(SimpleEnergyBuffer::new(capacity_fe)),
    );
}

/// Drop every capability handler at `(dim, x, y, z)` and forget the cell
/// in the registration table.
#[cfg(feature = "mod-suite")]
fn live_cap_unregister(dim: DimId, x: i32, y: i32, z: i32) {
    let coord = live_cap_coord(dim, x, y, z);
    live_cap_remove_endpoints(&mut live_cap_bus_guard(), &coord);
}

#[cfg(feature = "mod-suite")]
fn live_cap_remove_endpoints(bus: &mut BlockCapabilityBus, coord: &BlockCoord) {
    bus.unregister_item_handler(coord);
    bus.unregister_fluid_handler(coord);
    bus.unregister_energy_handler(coord);
}

/// The first non-empty endpoint set any loaded universal profile declares
/// for `block_id` (`None` when no profile gives the block a capability —
/// including builds without `create-offload`).
#[cfg(feature = "mod-suite")]
fn live_cap_profile_endpoints(
    block_id: &str,
) -> Option<mod_adapter_core::profile::ProfileCapabilityEndpoints> {
    #[cfg(feature = "create-offload")]
    {
        let bridge = mod_offload_bridge()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for profile in &bridge.profiles {
            let endpoints = profile.capability_endpoints_for(block_id);
            if !endpoints.is_empty() {
                return Some(endpoints);
            }
        }
    }
    #[cfg(not(feature = "create-offload"))]
    let _ = block_id;
    None
}

/// Register the endpoints `ModOffloadProfile::capability_endpoints_for`
/// declares for `block_id` at `(dim, x, y, z)` on the shared bus — the
/// first loaded profile declaring the block wins. `false` when the block
/// carries no declared capability.
#[cfg(feature = "mod-suite")]
fn live_cap_register_profile_block(dim: DimId, x: i32, y: i32, z: i32, block_id: &str) -> bool {
    let coord = live_cap_coord(dim, x, y, z);
    let Some(endpoints) = live_cap_profile_endpoints(block_id) else {
        return false;
    };
    let mut state = live_cap_state().lock().unwrap_or_else(|e| e.into_inner());
    let mut bus = live_cap_bus_guard();
    endpoints.register_into(&mut bus, &coord);
    state.profile_cells.insert(coord);
    true
}

/// Mirror a block write onto the capability-bus profile table: a plan
/// block id any loaded profile declares gets its endpoints (re)registered,
/// and a cell that stops being a capability block drops them. Called from
/// `live_create_on_block_set` after the Create runtime synced — a Create
/// state resolves to `None` here, so the two registration tables never
/// fight over a cell.
#[cfg(feature = "mod-suite")]
fn live_cap_sync_profile_endpoint(dim: DimId, x: i32, y: i32, z: i32, st: u32) {
    let coord = live_cap_coord(dim, x, y, z);
    let block_id = live_mod_name_for_state(st);
    let registered = block_id
        .as_deref()
        .is_some_and(|id| live_cap_register_profile_block(dim, x, y, z, id));
    if !registered {
        let mut state = live_cap_state().lock().unwrap_or_else(|e| e.into_inner());
        if state.profile_cells.remove(&coord) {
            live_cap_remove_endpoints(&mut live_cap_bus_guard(), &coord);
        }
    }
}

/// `coord` received items through the bus — when it addresses a live
/// cooking pot, re-arm its completion deadline so bus-fed ingredients
/// wake a sleeping pot like a manual insert would.
#[cfg(feature = "mod-suite")]
fn live_cap_notify_pot_fed(coord: &BlockCoord) {
    let mut suite = live_mod_suite().lock().unwrap_or_else(|e| e.into_inner());
    if !suite.active {
        return;
    }
    let tick = TICK_IDX.load(Ordering::Relaxed);
    let key = suite
        .cooking_pots
        .keys()
        .copied()
        .find(|&(d, x, y, z)| live_cap_coord(d, x, y, z) == *coord);
    if let Some(key) = key {
        suite.refresh_cooking_pot(key, tick);
    }
}

/// One cross-mod transfer pass: every armed Create duct pushes toward its
/// live downstream cell when that cell hosts a *foreign* bus item endpoint
/// (duct-to-duct flow stays on `SleepingTransport`), and Create fluid
/// pipes/pumps push toward adjacent fluid endpoints. Called from
/// `live_mod_tick`, so the ordinary game tick moves cargo between mods
/// with zero per-mod glue.
#[cfg(feature = "mod-suite")]
fn live_cap_bus_step() {
    let fed: Vec<BlockCoord> = live_create_with(|rt| rt.cap_bus_step()).unwrap_or_default();
    for coord in &fed {
        live_cap_notify_pot_fed(coord);
    }
}

/// Drop every registered endpoint and the registration bookkeeping —
/// suite (re)install and test isolation both run through here so a fresh
/// boot never inherits stale handlers.
#[cfg(feature = "mod-suite")]
fn live_cap_reset() {
    *live_cap_bus_guard() = BlockCapabilityBus::new();
    *live_cap_state().lock().unwrap_or_else(|e| e.into_inner()) = LiveCapState::default();
}

// ---------------------------------------------------------------------------
// Generic mod-block capability auto-registration (name-shape classification
// over JAR-declared block ids — never a mod-id literal).
// ---------------------------------------------------------------------------

/// The capability family a JAR-declared block id implies, classified from
/// the block path's shape vocabulary — the same generic convention
/// `mod_block_shape`/`capability_endpoints_for` already use, so any mod's
/// `*tank*`/`*generator*`/`*chest*` blocks expose bus endpoints without a
/// per-mod profile.
#[cfg(feature = "mod-suite")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GenericCapShape {
    /// Item slots — chest/barrel/crate/drawer/storage vocabulary.
    ItemStorage,
    /// Fluid capacity — tank/vat/reservoir/cistern vocabulary.
    FluidTank,
    /// Stored FE — battery/capacitor/accumulator vocabulary.
    EnergyBuffer,
    /// Energy source — generator/reactor vocabulary; also produces FE
    /// each tick through [`live_cap_receive_energy`].
    EnergyGenerator,
}

#[cfg(feature = "mod-suite")]
impl GenericCapShape {
    /// Remove only the endpoint kind this shape registered, preserving
    /// foreign handlers (cooking-pot adapters, profile endpoints) that may
    /// share the cell.
    fn unregister_from(self, bus: &mut BlockCapabilityBus, coord: &BlockCoord) {
        match self {
            Self::ItemStorage => {
                bus.unregister_item_handler(coord);
            }
            Self::FluidTank => {
                bus.unregister_fluid_handler(coord);
            }
            Self::EnergyBuffer | Self::EnergyGenerator => {
                bus.unregister_energy_handler(coord);
            }
        }
    }
}

/// Classify a JAR-declared block id into a generic capability shape, or
/// `None` when its path carries no capability vocabulary. Classification
/// order matters: containment words win over generation words so a
/// `fluid_generator` still reads as a tank, and storage words are checked
/// last so `item_pipe`-style transport names never become chests.
#[cfg(feature = "mod-suite")]
fn live_cap_generic_shape(block_id: &str) -> Option<GenericCapShape> {
    let path = block_id.rsplit(':').next().unwrap_or(block_id);
    const FLUID: &[&str] = &["tank", "vat", "reservoir", "cistern", "pool"];
    const ENERGY_GEN: &[&str] = &["generator", "reactor", "dynamos"];
    const ENERGY_STORE: &[&str] = &["battery", "capacitor", "accumulator", "energy_cell"];
    const STORAGE: &[&str] = &[
        "chest", "barrel", "crate", "drawer", "storage", "satchel", "backpack", "cabinet",
        "locker", "vault", "silo",
    ];
    if FLUID.iter().any(|k| path.contains(k)) {
        Some(GenericCapShape::FluidTank)
    } else if ENERGY_GEN.iter().any(|k| path.contains(k)) {
        Some(GenericCapShape::EnergyGenerator)
    } else if ENERGY_STORE.iter().any(|k| path.contains(k)) {
        Some(GenericCapShape::EnergyBuffer)
    } else if STORAGE.iter().any(|k| path.contains(k)) {
        Some(GenericCapShape::ItemStorage)
    } else {
        None
    }
}

/// Register the generic endpoints `shape` implies at `(dim, x, y, z)` on
/// the shared bus, sized by the same defaults `ModOffloadProfile`
/// endpoints use — zero mod-specific wiring.
#[cfg(feature = "mod-suite")]
fn live_cap_register_generic_shape(
    dim: DimId,
    x: i32,
    y: i32,
    z: i32,
    shape: GenericCapShape,
) {
    match shape {
        GenericCapShape::ItemStorage => live_cap_register_slot_inventory(
            dim,
            x,
            y,
            z,
            mod_adapter_core::profile::PROFILE_INVENTORY_SLOTS,
            mod_adapter_core::profile::PROFILE_MAX_STACK,
        ),
        GenericCapShape::FluidTank => live_cap_register_fluid_tank(
            dim,
            x,
            y,
            z,
            u64::from(mod_adapter_core::profile::DEFAULT_PROFILE_TANK_MB),
        ),
        GenericCapShape::EnergyBuffer | GenericCapShape::EnergyGenerator => {
            live_cap_register_energy_buffer(
                dim,
                x,
                y,
                z,
                mod_adapter_core::profile::DEFAULT_PROFILE_ENERGY_FE,
            );
        }
    }
}

/// Mirror a JAR-declared block write onto the capability bus: a plan
/// block whose id carries capability vocabulary gets its generic
/// endpoints (re)registered, a cell that stops being one drops exactly
/// the kind it registered, and a same-shape state flip (facing/powered)
/// keeps stored cargo because nothing is re-registered. Loaded
/// `ModOffloadProfile` declarations win — the generic layer yields to
/// `live_cap_sync_profile_endpoint`'s richer per-mod endpoints.
#[cfg(feature = "mod-suite")]
fn live_cap_sync_generic_endpoint(dim: DimId, x: i32, y: i32, z: i32, st: u32) {
    let coord = live_cap_coord(dim, x, y, z);
    let key = (dim, x, y, z);
    let shape = live_mod_name_for_state(st).and_then(|block_id| {
        if live_cap_profile_claims_block(&block_id) {
            None
        } else {
            live_cap_generic_shape(&block_id)
        }
    });
    let mut state = live_cap_state().lock().unwrap_or_else(|e| e.into_inner());
    match (shape, state.generic_cells.get(&key).copied()) {
        (Some(new), Some(old)) if new == old => {}
        (Some(new), old) => {
            if let Some(old) = old {
                old.unregister_from(&mut live_cap_bus_guard(), &coord);
            }
            live_cap_register_generic_shape(dim, x, y, z, new);
            state.generic_cells.insert(key, new);
        }
        (None, Some(old)) => {
            old.unregister_from(&mut live_cap_bus_guard(), &coord);
            state.generic_cells.remove(&key);
        }
        (None, None) => {}
    }
}

/// `true` when a loaded `ModOffloadProfile` declares `block_id` — the
/// profile layer's claim beats the generic name-shape classification.
#[cfg(feature = "mod-suite")]
fn live_cap_profile_claims_block(block_id: &str) -> bool {
    #[cfg(feature = "create-offload")]
    {
        let bridge = mod_offload_bridge()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if bridge.profiles.iter().any(|p| p.declares_capability_block(block_id)) {
            return true;
        }
    }
    #[cfg(not(feature = "create-offload"))]
    let _ = block_id;
    false
}

/// FE a generic generator produces per tick through its own buffer.
#[cfg(feature = "mod-suite")]
const LIVE_CAP_GENERIC_GEN_FE_PER_TICK: u64 = 40;

/// Generic endpoint cells visited per `live_cap_generic_step` — bounds
/// the per-tick transfer scan independently of how many mod storage
/// blocks a world accumulates.
#[cfg(feature = "mod-suite")]
const LIVE_CAP_GENERIC_STEP_CELLS: usize = 256;

/// One bounded cross-mod transfer pass over the generic endpoint table:
/// adjacent generic storage blocks share items, tanks and FE buffers
/// equalize through the same simulate-then-commit bus the Create pass
/// uses, and generator cells produce into their own buffers. Transfers
/// only flow toward the lexicographically larger neighbour coordinate so
/// two touching inventories can never ping-pong cargo every tick.
#[cfg(feature = "mod-suite")]
fn live_cap_generic_step() {
    let cells: Vec<((DimId, i32, i32, i32), GenericCapShape)> = {
        let state = live_cap_state().lock().unwrap_or_else(|e| e.into_inner());
        state
            .generic_cells
            .iter()
            .take(LIVE_CAP_GENERIC_STEP_CELLS)
            .map(|(key, shape)| (*key, *shape))
            .collect()
    };
    if cells.is_empty() {
        return;
    }
    for ((dim, x, y, z), shape) in cells {
        if shape == GenericCapShape::EnergyGenerator {
            let _ = live_cap_receive_energy(
                dim,
                x,
                y,
                z,
                LIVE_CAP_GENERIC_GEN_FE_PER_TICK,
                false,
            );
        }
        for (dx, dy, dz) in [(1, 0, 0), (-1, 0, 0), (0, 1, 0), (0, -1, 0), (0, 0, 1), (0, 0, -1)] {
            let (nx, ny, nz) = (x + dx, y + dy, z + dz);
            // Directional gate: only the lower-sorting cell pushes, so a
            // pair never trades the same cargo back and forth per tick.
            if (nx, ny, nz) <= (x, y, z) {
                continue;
            }
            let from_face = live_cap_face_of((dx, dy, dz));
            let to_face = live_cap_face_of((-dx, -dy, -dz));
            match shape {
                GenericCapShape::ItemStorage => {
                    let _ = live_cap_transfer_adjacent(
                        dim,
                        [x, y, z],
                        from_face,
                        [nx, ny, nz],
                        to_face,
                        LIVE_CAP_STEP_ITEMS,
                    );
                }
                GenericCapShape::FluidTank => {
                    let _ = live_cap_transfer_fluid(
                        dim,
                        [x, y, z],
                        from_face,
                        [nx, ny, nz],
                        to_face,
                        LIVE_CAP_STEP_FLUID_MB,
                    );
                }
                GenericCapShape::EnergyBuffer | GenericCapShape::EnergyGenerator => {
                    let _ = live_cap_transfer_energy(
                        dim,
                        [x, y, z],
                        from_face,
                        [nx, ny, nz],
                        to_face,
                        LIVE_CAP_GENERIC_GEN_FE_PER_TICK,
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// `megu:gui_v1` (`GGB1`) widget-batch producer — the server-side encoder
// for the client-ui draw-op wire format, so mod workstations/HUDs push
// widget batches over the custom-payload channel with zero per-mod glue.
// ---------------------------------------------------------------------------

/// Custom-payload channel carrying [`live_mod_gui_v1_encode`] output.
#[cfg(feature = "mod-suite")]
pub const LIVE_MOD_GUI_V1_CHANNEL: &str = "megu:gui_v1";

/// Wire magic shared with `client-ui`'s `gui_bridge` codec.
#[cfg(feature = "mod-suite")]
const LIVE_MOD_GUI_V1_MAGIC: &[u8; 4] = b"GGB1";
#[cfg(feature = "mod-suite")]
const LIVE_MOD_GUI_V1_VERSION: u8 = 1;
#[cfg(feature = "mod-suite")]
const LIVE_MOD_GUI_V1_MAX_OPS: usize = 2048;
#[cfg(feature = "mod-suite")]
const LIVE_MOD_GUI_V1_MAX_STRING: usize = 512;

/// One draw op in a `megu:gui_v1` batch — the server-side mirror of
/// `client-ui`'s `GuiBridgeDrawOp`. Tags and field order are identical so
/// a batch encoded here decodes through `gui_bridge::decode_batch`.
#[cfg(feature = "mod-suite")]
#[derive(Clone, Debug, PartialEq)]
pub enum LiveModGuiOp {
    /// Filled rectangle.
    FillRect {
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        rgba: [u8; 4],
    },
    /// Monospace bitmap text.
    DrawText {
        x: i32,
        y: i32,
        scale: u8,
        rgba: [u8; 4],
        text: String,
    },
    /// A vanilla 16×16 item icon plus stack-count badge.
    DrawItemIcon {
        x: i32,
        y: i32,
        item_id: String,
        count: u8,
    },
    /// Clip subsequent ops to this rect until the matching `PopScissor`.
    PushScissor {
        x: i32,
        y: i32,
        w: u32,
        h: u32,
    },
    /// Restore the enclosing clip.
    PopScissor,
}

/// Why [`live_mod_gui_v1_encode`] refused a batch — mirrors the
/// client-ui codec's fail-closed rules.
#[cfg(feature = "mod-suite")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LiveModGuiEncodeError {
    /// More ops than the wire format's `u16` count / op cap allows.
    TooManyOps(usize),
    /// A string field exceeded the 512-byte wire bound.
    StringTooLong(usize),
    /// `PopScissor` arrived with nothing on the stack.
    ScissorUnderflow,
    /// The batch ended with `PushScissor`s still open.
    UnbalancedScissors,
}

#[cfg(feature = "mod-suite")]
fn live_mod_gui_v1_push_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}

#[cfg(feature = "mod-suite")]
fn live_mod_gui_v1_push_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

#[cfg(feature = "mod-suite")]
fn live_mod_gui_v1_push_i32(out: &mut Vec<u8>, v: i32) {
    out.extend_from_slice(&v.to_le_bytes());
}

#[cfg(feature = "mod-suite")]
fn live_mod_gui_v1_push_str(out: &mut Vec<u8>, s: &str) -> Result<(), LiveModGuiEncodeError> {
    if s.len() > LIVE_MOD_GUI_V1_MAX_STRING {
        return Err(LiveModGuiEncodeError::StringTooLong(s.len()));
    }
    live_mod_gui_v1_push_u16(out, s.len() as u16);
    out.extend_from_slice(s.as_bytes());
    Ok(())
}

/// Encode `ops` into a `GGB1` (`megu:gui_v1`) widget batch — the exact
/// wire form `client-ui`'s `gui_bridge::decode_batch` accepts: magic,
/// version, flags, `u16` op count, then length-prefixed ops. Encoding is
/// fail-closed: an over-cap op count, overlong string, or unbalanced
/// scissor stack returns `Err` instead of emitting an undecodable frame.
#[cfg(feature = "mod-suite")]
pub fn live_mod_gui_v1_encode(ops: &[LiveModGuiOp]) -> Result<Vec<u8>, LiveModGuiEncodeError> {
    if ops.len() > LIVE_MOD_GUI_V1_MAX_OPS {
        return Err(LiveModGuiEncodeError::TooManyOps(ops.len()));
    }
    let mut depth = 0usize;
    for op in ops {
        match op {
            LiveModGuiOp::PushScissor { .. } => depth += 1,
            LiveModGuiOp::PopScissor => {
                depth = depth
                    .checked_sub(1)
                    .ok_or(LiveModGuiEncodeError::ScissorUnderflow)?;
            }
            LiveModGuiOp::DrawText { text, .. } => {
                if text.len() > LIVE_MOD_GUI_V1_MAX_STRING {
                    return Err(LiveModGuiEncodeError::StringTooLong(text.len()));
                }
            }
            LiveModGuiOp::DrawItemIcon { item_id, .. } => {
                if item_id.len() > LIVE_MOD_GUI_V1_MAX_STRING {
                    return Err(LiveModGuiEncodeError::StringTooLong(item_id.len()));
                }
            }
            LiveModGuiOp::FillRect { .. } => {}
        }
    }
    if depth != 0 {
        return Err(LiveModGuiEncodeError::UnbalancedScissors);
    }

    let mut out = Vec::with_capacity(8 + ops.len() * 24);
    out.extend_from_slice(LIVE_MOD_GUI_V1_MAGIC);
    out.push(LIVE_MOD_GUI_V1_VERSION);
    out.push(0); // flags (reserved, must be 0)
    live_mod_gui_v1_push_u16(&mut out, ops.len() as u16);
    for op in ops {
        match op {
            LiveModGuiOp::FillRect { x, y, w, h, rgba } => {
                out.push(0x01);
                live_mod_gui_v1_push_i32(&mut out, *x);
                live_mod_gui_v1_push_i32(&mut out, *y);
                live_mod_gui_v1_push_u32(&mut out, *w);
                live_mod_gui_v1_push_u32(&mut out, *h);
                out.extend_from_slice(rgba);
            }
            LiveModGuiOp::DrawText {
                x,
                y,
                scale,
                rgba,
                text,
            } => {
                out.push(0x02);
                live_mod_gui_v1_push_i32(&mut out, *x);
                live_mod_gui_v1_push_i32(&mut out, *y);
                out.push(*scale);
                out.extend_from_slice(rgba);
                live_mod_gui_v1_push_str(&mut out, text)?;
            }
            LiveModGuiOp::DrawItemIcon {
                x,
                y,
                item_id,
                count,
            } => {
                out.push(0x04);
                live_mod_gui_v1_push_i32(&mut out, *x);
                live_mod_gui_v1_push_i32(&mut out, *y);
                live_mod_gui_v1_push_str(&mut out, item_id)?;
                out.push(*count);
            }
            LiveModGuiOp::PushScissor { x, y, w, h } => {
                out.push(0x05);
                live_mod_gui_v1_push_i32(&mut out, *x);
                live_mod_gui_v1_push_i32(&mut out, *y);
                live_mod_gui_v1_push_u32(&mut out, *w);
                live_mod_gui_v1_push_u32(&mut out, *h);
            }
            LiveModGuiOp::PopScissor => out.push(0x06),
        }
    }
    Ok(out)
}

/// Build the `megu:gui_v1` widget batch describing a live cooking pot:
/// panel background, title, one item icon per occupied input/container/
/// output slot, and a cook-progress line. Deterministic and bounded —
/// at most `POT_INPUT_SLOTS + 2` icon ops regardless of contents.
#[cfg(feature = "mod-suite")]
fn live_mod_gui_v1_cooking_pot_batch(pot: &CookingPotState) -> Vec<u8> {
    let mut ops = Vec::with_capacity(16);
    ops.push(LiveModGuiOp::FillRect {
        x: 0,
        y: 0,
        w: 176,
        h: 166,
        rgba: [0x20, 0x20, 0x28, 0xff],
    });
    ops.push(LiveModGuiOp::DrawText {
        x: 8,
        y: 6,
        scale: 1,
        rgba: [0xf0, 0xf0, 0xf0, 0xff],
        text: "Cooking Pot".to_string(),
    });
    let mut slot_index = 0usize;
    let mut icon = |ops: &mut Vec<LiveModGuiOp>, slot: &Option<(String, u16)>| {
        if let Some((item, count)) = slot {
            let col = slot_index % 3;
            let row = slot_index / 3;
            ops.push(LiveModGuiOp::DrawItemIcon {
                x: 8 + col as i32 * 18,
                y: 22 + row as i32 * 18,
                item_id: item.clone(),
                count: u8::try_from(*count).unwrap_or(u8::MAX),
            });
            slot_index += 1;
        }
    };
    for input in &pot.inputs {
        icon(&mut ops, input);
    }
    icon(&mut ops, &pot.container);
    icon(&mut ops, &pot.output);
    let status = if pot.heated {
        match pot.next_completion_tick {
            Some(_) => "cooking",
            None => "heated — no recipe",
        }
    } else {
        "unheated"
    };
    ops.push(LiveModGuiOp::DrawText {
        x: 8,
        y: 140,
        scale: 1,
        rgba: [0xa0, 0xa0, 0xa0, 0xff],
        text: status.to_string(),
    });
    live_mod_gui_v1_encode(&ops).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Generic mod-block `on_use` dispatch — one table every modded workstation
// routes through instead of bespoke per-mod handlers.
// ---------------------------------------------------------------------------

/// What [`live_mod_block_on_use`] resolved for the clicked cell.
#[cfg(feature = "mod-suite")]
#[derive(Clone, Debug, PartialEq)]
pub enum LiveModUseOutcome {
    /// A `*cooking_pot` workstation took `held_item` into its input slots;
    /// `gui` is the ready-to-send `megu:gui_v1` widget batch.
    CookingPot { accepted: u32, gui: Vec<u8> },
    /// A `*cutting_board`-style workstation ran `held_item` under
    /// `held_tool` through the shared cutting-recipe table.
    Cutting(CuttingOutcome),
    /// The cell's generic capability endpoint accepted `held_item`
    /// through the shared bus.
    CapabilityFeed { accepted: u32 },
    /// A fluid endpoint exchanged `held_item` (a `*_bucket` or plain
    /// `bucket`) for `fluid_mb` of `fluid_id`.
    FluidExchange {
        /// mB moved into (`fill`) or out of (`drain`) the endpoint.
        fluid_mb: u64,
        /// Namespaced fluid id exchanged, when known.
        fluid_id: Option<String>,
    },
}

/// Resolve the canonical block name for any live state id — JAR plan
/// blocks first, then the vanilla/Create-aware registry table.
#[cfg(feature = "mod-suite")]
fn live_mod_block_name_of(st: u32) -> String {
    live_mod_name_for_state(st)
        .or_else(|| breg().map(|reg| reg.name_of(st).to_string()))
        .unwrap_or_else(|| "minecraft:air".to_string())
}

/// Right-click a live cooking pot at `(dim, x, y, z)` with `held_item`:
/// pushes the stack through the shared bus endpoint (the same
/// simulate-then-commit path Create ducts use, so the canonical pot
/// state — not a shadow copy — receives it), then returns the accepted
/// count plus the pot's `megu:gui_v1` widget batch for the caller to
/// push over [`LIVE_MOD_GUI_V1_CHANNEL`]. `None` when no live pot
/// occupies the cell.
#[cfg(feature = "mod-suite")]
fn live_mod_cooking_pot_use(
    dim: DimId,
    x: i32,
    y: i32,
    z: i32,
    held_item: &str,
) -> Option<(u32, Vec<u8>)> {
    let batch = {
        let suite = live_mod_suite().lock().unwrap_or_else(|e| e.into_inner());
        if !suite.active {
            return None;
        }
        let adapter = suite.cooking_pots.get(&(dim, x, y, z))?;
        let guard = adapter.lock().unwrap_or_else(|e| e.into_inner());
        live_mod_gui_v1_cooking_pot_batch(guard.pot())
    };
    let accepted = live_cap_insert_item(dim, x, y, z, None, held_item, 1, &[], false);
    Some((accepted, batch))
}

/// Generic mod-block `on_use` dispatch — the single table every modded
/// workstation routes through instead of per-mod handlers. Consulted in
/// order: cooking-pot workstations (bus insert + `megu:gui_v1` batch),
/// cutting-board workstations (shared recipe arbitration), fluid
/// endpoints (`*_bucket` pour/drain), then any other capability endpoint
/// (generic item feed). `held_item`/`held_tool` are canonical `ns:path`
/// ids; an empty hand passes `""`. `seed` feeds the cutting recipe's
/// deterministic output roll. `None` means no mod handler claimed the
/// click — the caller falls through to vanilla placement/interact.
#[cfg(feature = "mod-suite")]
pub fn live_mod_block_on_use(
    dim: u32,
    x: i32,
    y: i32,
    z: i32,
    held_item: &str,
    held_tool: &str,
    seed: u64,
) -> Option<LiveModUseOutcome> {
    let dim = DimId::from_raw(dim);
    if !live_mod_suite()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .active
    {
        return None;
    }
    let block_id = live_mod_block_name_of(block_at_dim(dim, x, y, z));
    if block_id.ends_with("cooking_pot") {
        if let Some((accepted, gui)) = live_mod_cooking_pot_use(dim, x, y, z, held_item) {
            return Some(LiveModUseOutcome::CookingPot { accepted, gui });
        }
    }
    if block_id.ends_with("cutting_board") && !held_item.is_empty() {
        if let Some(outcome) = live_mod_cutting_board_use(held_item, held_tool, seed) {
            return Some(LiveModUseOutcome::Cutting(outcome));
        }
    }
    // Bucket interactions with fluid endpoints: a `ns:*_bucket` fills the
    // tank with its namesake fluid (`minecraft:water_bucket` -> `minecraft:water`);
    // a plain `minecraft:bucket` drains 1000 mB back out.
    if let Some(fluid_base) = held_item.strip_suffix("_bucket") {
        let namespaced = fluid_base.contains(':')
            && fluid_base.rsplit(':').next().is_some_and(|p| !p.is_empty());
        if namespaced {
            let accepted = live_cap_fill_fluid(dim, x, y, z, fluid_base, 1_000, &[], false);
            if accepted > 0 {
                return Some(LiveModUseOutcome::FluidExchange {
                    fluid_mb: accepted,
                    fluid_id: Some(fluid_base.to_string()),
                });
            }
        }
    } else if held_item == "minecraft:bucket" {
        if let Some(drained) = live_cap_drain_fluid(dim, x, y, z, 1_000, false) {
            return Some(LiveModUseOutcome::FluidExchange {
                fluid_mb: drained.amount_mb,
                fluid_id: Some(drained.fluid_id),
            });
        }
    }
    if !held_item.is_empty() {
        let accepted = live_cap_insert_item(dim, x, y, z, None, held_item, 1, &[], false);
        if accepted > 0 {
            return Some(LiveModUseOutcome::CapabilityFeed { accepted });
        }
    }
    None
}

/// Equip for a single player without affecting any other player's slots.
#[cfg(feature = "mod-suite")]
pub fn live_mod_equip_accessory(
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
pub fn live_mod_accessory_attributes(player: &str) -> BTreeMap<String, AttributeTotals> {
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
pub fn live_mod_sync_root_hash_probe() -> Option<[u8; 36]> {
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
pub fn live_mod_sync_manifest_for_client_probe(client_probe: Option<&[u8]>) -> Option<Vec<u8>> {
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

#[cfg(feature = "mod-suite")]
fn live_mod_send_payload(
    socket: &mut TcpStream,
    channel: &str,
    payload: &[u8],
) -> std::io::Result<()> {
    send_packet(
        socket,
        LIVE_MOD_S2C_CUSTOM_PAYLOAD_ID,
        live_mod_payload_body(channel, payload),
    )
}

/// The season payload channel requested by the server bridge's wire contract.
#[cfg(feature = "mod-suite")]
pub const LIVE_MOD_SEASON_CHANNEL: &str = "megu:season";

/// Assemble the deterministic SSN1 season snapshot for the selected biome
/// temperature. Tints use the representative plains biome because the bridge
/// currently receives only the temperature, rather than a biome identifier.
#[cfg(feature = "mod-suite")]
pub fn live_mod_season_sync_frame(world_ticks: u64, biome_temp: f32) -> Option<(String, Vec<u8>)> {
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
pub fn live_mod_emit_season_sync(cid: u64, world_ticks: u64, biome_temp: f32) -> bool {
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
        live_mod_send_payload(&mut socket, &channel, &payload).is_ok()
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
pub fn live_mod_roll_halloween_headgear(
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
pub fn live_mod_chest_holiday_variant(block_id: &str, month: u8, day: u8) -> &'static str {
    let Some(date) = CalendarDate::new(month, day) else {
        return "standard";
    };
    match chest_holiday_variant(block_id, date, &HolidayConfig::default()) {
        ChestVisualVariant::Standard => "standard",
        ChestVisualVariant::ChristmasPresent => "christmas_present",
    }
}

#[cfg(feature = "mod-suite")]
#[allow(clippy::too_many_arguments)]
pub fn live_mod_try_qol_right_click_harvest(
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
#[derive(Debug, PartialEq)]
pub enum LiveMagicError {
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
#[derive(Debug, Default)]
pub struct LiveMagicTick {
    pub cast_events: Vec<lmb_magic::CastEvent>,
    pub spell_plans: Vec<lmb_magic::SpellExecutionResult>,
    pub execution_errors: Vec<lmb_magic::SpellExecutionError>,
    pub statuses: lmb_magic::StatusTickReport,
    pub crowd_control: lmb_magic::CcFlags,
    pub mana_delta: f32,
    pub cooldowns_changed: bool,
    pub frame: Option<Vec<u8>>,
}

#[cfg(feature = "mod-suite")]
#[derive(Debug, Default)]
pub struct LiveMagicCast {
    pub events: Vec<lmb_magic::CastEvent>,
    pub spell_plans: Vec<lmb_magic::SpellExecutionResult>,
}

/// Configure a player without wiping casts, cooldowns, or statuses on repeat
/// calls. Current mana is clamped to the new maximum.
#[cfg(feature = "mod-suite")]
pub fn live_mod_magic_set_attributes(
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
pub fn live_mod_magic_register_status(
    definition: lmb_magic::StatusEffectDefinition,
) -> Result<(), lmb_magic::StatusDefError> {
    live_magic_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .registry
        .register(definition)
}

#[cfg(feature = "mod-suite")]
pub fn live_mod_magic_apply_status(
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
pub fn live_mod_magic_absorb_damage(uuid: [u8; 16], damage: f32) -> f32 {
    let mut magic = live_magic_state().lock().unwrap_or_else(|e| e.into_inner());
    magic
        .players
        .get_mut(&uuid)
        .map_or(damage, |player| player.effects.absorb_damage(damage))
}

/// Refuse invalid, silenced or unaffordable casts atomically. Instants return
/// their resolved effect plans now; timed/channeled casts resolve on ticks.
#[cfg(feature = "mod-suite")]
pub fn live_mod_magic_cast(
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

/// Tick players opted into magic by a cast or explicit configuration.
/// A server-wide status registry also enables magic for joining players.
#[cfg(feature = "mod-suite")]
fn live_mod_magic_tick_player(uuid: [u8; 16]) -> Option<LiveMagicTick> {
    let mut magic = live_magic_state().lock().unwrap_or_else(|e| e.into_inner());
    if !magic.registry.is_empty() {
        magic.player_mut(uuid).ok()?;
    }
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
pub fn live_mod_magic_remove_player(uuid: [u8; 16]) -> bool {
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

/// Vanilla's only multi-part entity — the bridge registers its part
/// layout once at suite boot so melee rays are tested against real
/// sub-part AABBs instead of the whole-entity box.
const LIVE_ENDER_DRAGON_ID: &str = "minecraft:ender_dragon";

/// `EnderDragon`'s part layout mirrored into bone-local offsets
/// `(left, up, forward)`: head/neck forward of the body centre, thin
/// wing plates out to the sides and three tail segments trailing back.
#[cfg(feature = "mod-suite")]
fn live_ender_dragon_hitbox() -> MultiPartHitbox {
    let part =
        |kind: PartKind, lx: f64, uy: f64, fz: f64, hx: f64, hy: f64, hz: f64| PartOffsetSpec {
            kind,
            local_offset: [lx, uy, fz],
            half_extents: [hx, hy, hz],
            damage_multiplier: None,
        };
    MultiPartHitbox {
        parts: vec![
            part(PartKind::Head, 0.0, 1.0, 6.0, 0.5, 0.5, 0.5),
            part(PartKind::Neck, 0.0, 0.9, 4.5, 0.5, 0.5, 1.0),
            part(PartKind::Torso, 0.0, 0.5, 0.0, 1.5, 1.0, 2.5),
            part(PartKind::LeftWing, -4.5, 1.2, 0.0, 2.5, 0.2, 1.0),
            part(PartKind::RightWing, 4.5, 1.2, 0.0, 2.5, 0.2, 1.0),
            part(PartKind::Tail(0), 0.0, 0.4, -4.0, 0.6, 0.6, 1.5),
            part(PartKind::Tail(1), 0.0, 0.3, -7.0, 0.5, 0.5, 1.5),
            part(PartKind::Tail(2), 0.0, 0.2, -10.0, 0.4, 0.4, 1.5),
        ],
    }
}

/// `true` when `dir` is a readable directory containing at least one
/// `.jar` mod or `.bundle.json` mod bundle — the "is this a real mods
/// folder" probe for the conventional `./mods` fallback so an absent,
/// empty, or unrelated `mods/` directory keeps vanilla's zero-mod boot.
#[cfg(feature = "mod-suite")]
fn live_mod_conventional_mods_dir(dir: &std::path::Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        name.ends_with(".jar") || name.ends_with(".bundle.json")
    })
}

/// Startup entry wired from `main`: scans `MEGU_MODS_DIR` when set, else
/// the conventional `./mods` directory when it actually holds mod
/// artifacts, then arms the optional operator manifest (`MEGU_MOD_SYNC`)
/// and progression rules (`MEGU_PROGRESSION`). Without any mods to scan
/// or manifest to arm the bridge stays completely inert — same as a
/// default build.
#[cfg(feature = "mod-suite")]
fn live_mod_suite_boot() {
    // The optimizer's memoizer/coalescer/batcher state is per-boot, and
    // `MEGU_LAZY_CATCHUP` is re-read so a config flip on re-arm takes
    // effect without a process restart.
    live_auto_optimizer_reset();
    live_mod_lazy_catchup_set_enabled(
        std::env::var("MEGU_LAZY_CATCHUP")
            .map(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false),
    );
    // Vanilla's canonical multi-part entity arms the colossal/breath
    // surface: the ender dragon's part layout feeds
    // `live_colossal_hit_test`, its fire breath feeds
    // `live_breath_evaluate`/`live_forge_apply_breath`.
    live_colossal_unregister_hitbox(LIVE_ENDER_DRAGON_ID);
    live_colossal_register_hitbox(LIVE_ENDER_DRAGON_ID, live_ender_dragon_hitbox());
    live_breath_register(LIVE_ENDER_DRAGON_ID, ElementalBreathSpec::fire());
    // Conventional server layout: with `MEGU_MODS_DIR` unset, a `./mods`
    // directory holding `.jar`/`.bundle.json` artifacts is scanned — the
    // same place Fabric/Forge servers look — while an absent or empty
    // one preserves the zero-mod early return below.
    let mods_dir = std::env::var_os("MEGU_MODS_DIR")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            let conventional = std::path::PathBuf::from("mods");
            live_mod_conventional_mods_dir(&conventional).then_some(conventional)
        });
    let sync_path = std::env::var_os("MEGU_MOD_SYNC");
    let progression_path = std::env::var_os("MEGU_PROGRESSION");
    if mods_dir.is_none() && sync_path.is_none() && progression_path.is_none() {
        return;
    }
    let mut suite = LiveModSuite::new();
    if let Some(dir) = mods_dir.as_deref() {
        suite.scan_mods_dir(dir);
    }
    if let Some(path) = sync_path.as_deref() {
        suite.arm_mod_sync_json(std::path::Path::new(path));
    }
    if let Some(path) = progression_path.as_deref() {
        suite.load_progression_json(std::path::Path::new(path));
    }
    suite.active = true;
    *live_mod_suite().lock().unwrap_or_else(|e| e.into_inner()) = suite;
    // A fresh boot never inherits capability endpoints from a previous
    // suite lifetime.
    live_cap_reset();
    live_mod_refresh_hook_gate();
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
    // AUTO-OPT: the delta coalescer suppresses byte-identical repeats the
    // per-player trackers let through — keyed per (player, channel order)
    // so exhaustion and saturation each keep their own cadence.
    let tick = TICK_IDX.load(Ordering::Relaxed);
    if let Ok(mut sock) = client.sock.lock() {
        for (order, body) in bodies.into_iter().enumerate() {
            let source = ((client.eid as u64) << 8) | order as u64;
            if !live_mod_delta_should_emit(source, &body, tick, 0) {
                continue;
            }
            let _ = send_packet(&mut sock, LIVE_MOD_S2C_CUSTOM_PAYLOAD_ID, body);
        }
    }
}

/// Per-game-tick entry wired into `game_tick` as `run("live_mods", ...)`.
/// The `Arc`s are cloned before any socket write. Magic players are advanced
/// independently of the optional AppleSkin suite and expired on disconnect.
#[cfg(feature = "mod-suite")]
fn live_mod_tick() {
    // Registered `ServerTickPre` bindings observe the tick head behind the
    // same one-atomic gate as damage dispatch; cooking pots advance on
    // their armed deadlines (empty scheduler = no work).
    live_mod_fire_hook(StandardEventHook::ServerTickPre);
    let _completed = live_mod_tick_cooking_pots(TICK_IDX.load(Ordering::Relaxed));
    // XMOD1: one cross-mod transfer pass — armed Create ducts push into
    // foreign bus endpoints (cooking pots, profile blocks, generic mod
    // storage) and fluid cells push toward adjacent tanks, all through
    // the same simulate-then-commit bus.
    live_cap_bus_step();
    // Generic capability cells: JAR-declared `*tank*`/`*generator*`/`*chest*`
    // blocks equalize cargo with each other and generators feed their own
    // buffers — bounded scan, empty table costs one lock + is_empty.
    live_cap_generic_step();
    // Tier-25 guarded batches drain once per tick so submitted records
    // never accumulate while no loader host consumes the frames.
    for lane in [
        megu_core::tier25_guard_batch::LoaderLane::NeoForge,
        megu_core::tier25_guard_batch::LoaderLane::Fabric,
    ] {
        if let Some(frame) = live_tier25_flush_tick(TICK_IDX.load(Ordering::Relaxed), lane) {
            eprintln!(
                "[live-mods] tier25 batch flushed: {} byte(s) lane {:?}",
                frame.len(),
                lane
            );
        }
    }
    // AUTO-OPT: headless factory nodes advance one tick — empty registry
    // and disabled limits both cost a single map-length probe.
    let _ = live_mod_headless_factory_step(1);
    // Burst writes recorded through `live_block_changed` flush once per
    // tick: each surviving section cell ships its net light delta to the
    // engine as one note instead of one edit per write.
    for batch in live_mod_bulk_drain() {
        if !batch.dirty_light {
            continue;
        }
        let dim = DimId::from_raw(batch.key.dim as u32);
        for cell in &batch.cells {
            let x = batch.key.cx * 16 + i32::from(cell.index & 15);
            let z = batch.key.cz * 16 + i32::from((cell.index >> 4) & 15);
            let y = batch.key.sy * 16 + i32::from(cell.index >> 8);
            note_light_edit(dim, Position::new(x, y, z), cell.old_state, cell.new_state);
        }
    }
    // Registered mod forges tick their idle heat decay / smelt advance once
    // per server tick — bounded by the endpoint count, not the world size.
    for coord in live_forge_cores(|_| true) {
        if let Some(dim) = live_dim_for_coord(&coord) {
            let _ = live_forge_tick(dim, coord.x, coord.y, coord.z);
        }
    }
    // Periodic optimizer diagnostic — every five minutes at 20 tps.
    let tick = TICK_IDX.load(Ordering::Relaxed);
    if tick.is_multiple_of(6000) {
        let (stale_chunks, compacted_bytes, projection_p99, overlay_p99, overlay_entries) =
            live_server_opt_summary();
        eprintln!(
            "[live-mods] auto-optimizer: {} headless endpoints, stats {:?}; server-opt stale_chunks={} compacted_bytes={} projection_p99={:.3}ms overlay_p99={:.3}ms overlay_entries={}",
            live_mod_headless_factory_len(),
            live_mod_auto_optimizer_stats(),
            stale_chunks,
            compacted_bytes,
            projection_p99,
            overlay_p99,
            overlay_entries,
        );
    }
    let players = clients().lock().unwrap_or_else(|e| e.into_inner()).clone();
    let connected: HashSet<[u8; 16]> = players.iter().map(|client| client.uuid).collect();
    {
        let connected_sessions: HashSet<(i32, [u8; 16])> = players
            .iter()
            .map(|client| (client.eid, client.uuid))
            .collect();
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
        if let Some(frame) = live_mod_magic_tick_player(client.uuid).and_then(|tick| tick.frame) {
            live_mod_magic_send_frame(client, &frame);
        }
        live_mod_appleskin_sync(client);
    }
    live_mod_fire_hook(StandardEventHook::ServerTickPost);
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
    if let Some(msy1) = body {
        let _ = live_mod_send_payload(s, lmb_sync::MOD_SYNC_CHANNEL, &msy1);
    }
}

/// Encode the live dynamic mod registry as one `MRS1`
/// (`megu:mod_registry_sync_v1`) frame for a joining client: Create's armed
/// block-state/item ids (states at `DEFAULT_BASE_STATE_ID`+, items at
/// `LIVE_CREATE_ITEM_BASE`+) plus the generic `MEGU_MODS_DIR` registry plan
/// built at boot. `None` when nothing modded is registered, so a vanilla or
/// unarmed server emits zero extra packets.
///
/// The encoded frame is cached inside the suite keyed by `Mrs1FrameKey` —
/// registry revision plus the shape of every contributing table — so repeat
/// joins hand back the same `Arc` instead of re-sorting and re-encoding the
/// whole registry. `scan_mods_dir` bumps `registry_revision` and drops the
/// cache, so a content change forces exactly one rebuild.
#[cfg(feature = "mod-suite")]
fn live_mod_registry_sync_frame() -> Option<Arc<[u8]>> {
    let mut block_states: Vec<DynamicBlockStateEntry> = Vec::new();
    let mut items: BTreeMap<u32, String> = BTreeMap::new();
    let mut bytecode_facts = BTreeMap::new();

    if live_create_enabled() {
        if let Some(registry) = breg().and_then(|reg| reg.create_registry()) {
            for entry in registry.states() {
                let mut properties = entry.properties.clone();
                properties.sort_by(|a, b| a.0.cmp(&b.0));
                block_states.push(DynamicBlockStateEntry {
                    state_id: entry.state_id,
                    block_id: entry.block_id.clone(),
                    properties,
                    facts_override: None,
                });
            }
        }
        for (index, (name, _)) in live_create_item_table().iter().enumerate() {
            let id = u32::try_from(LIVE_CREATE_ITEM_BASE + index as i64).ok()?;
            items.insert(id, name.clone());
        }
    }

    let mut suite = live_mod_suite().lock().unwrap_or_else(|e| e.into_inner());
    let key = Mrs1FrameKey {
        registry_revision: suite.registry_revision,
        create_enabled: live_create_enabled(),
        create_state_count: block_states.len(),
        plan_state_count: suite
            .registry_plan
            .as_ref()
            .map_or(0, |plan| plan.block_states.len()),
        plan_item_count: suite
            .registry_plan
            .as_ref()
            .map_or(0, |plan| plan.items.len()),
    };
    if let Some(cache) = suite.mrs1_cache.as_ref() {
        if cache.key == key {
            return cache.frame.clone();
        }
    }

    if let Some(plan) = suite.registry_plan.as_ref() {
        block_states.extend(plan.block_states.iter().cloned());
        items.extend(plan.items.iter().map(|(id, name)| (*id, name.clone())));
        bytecode_facts.extend(plan.bytecode_facts.iter().map(|(k, v)| (k.clone(), *v)));
    }

    let frame = if block_states.is_empty() && items.is_empty() {
        None
    } else {
        block_states.sort_by_key(|entry| entry.state_id);
        let base_block_state_id = block_states.first().map_or(0, |entry| entry.state_id);
        let base_item_id = items.keys().next().copied().unwrap_or(0);
        let plan = UnifiedModRegistryPlan {
            base_block_state_id,
            base_item_id,
            block_states,
            items,
            content: RegistryContentIndex::default(),
            bytecode_facts,
        };
        // v2 is the default wire shape — it carries the luminance/opacity
        // facts clients need to render modded blocks. `MEGU_MRS1_LEGACY_V1=1`
        // pins the pre-v2 frame for deployments whose clients cannot parse
        // the v2 tail; `decode_registry_sync_frame` accepts either.
        let frame_bytes = if std::env::var("MEGU_MRS1_LEGACY_V1").is_ok_and(|v| v == "1") {
            plan.encode_registry_sync_frame()
        } else {
            plan.encode_registry_sync_frame_v2()
        };
        Some(frame_bytes.into())
    };
    suite.mrs1_cache = Some(Mrs1FrameCache {
        key,
        frame: frame.clone(),
    });
    frame
}

/// Serve the `megu:mod_registry_sync_v1` (`MRS1`) dynamic registry frame to
/// a client that just entered play, wired next to the Create kinetic
/// snapshot. A no-op on vanilla/unarmed servers — `None` frame, no packet.
#[cfg(feature = "mod-suite")]
fn live_mod_registry_sync_on_join(s: &mut TcpStream) {
    if let Some(frame) = live_mod_registry_sync_frame() {
        let _ = live_mod_send_payload(s, MOD_REGISTRY_SYNC_CHANNEL, &frame);
    }
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
pub fn live_progression_grant(player: &str, advancement_id: &str) -> bool {
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

#[cfg(feature = "mod-suite")]
fn live_mod_json_rows<T>(
    bytes: &[u8],
    key: &str,
    mut parse: impl FnMut(usize, &J) -> Result<T, String>,
) -> Result<Vec<T>, String> {
    let root = jparse(bytes).ok_or_else(|| "invalid JSON".to_string())?;
    let rows = json::jarr(&root, key).ok_or_else(|| format!("missing '{key}' array"))?;
    let mut out = Vec::with_capacity(rows.len());
    for (index, row) in rows.iter().enumerate() {
        out.push(parse(index, row)?);
    }
    Ok(out)
}

/// Parse the mod-sync manifest JSON described at `arm_mod_sync_json`.
#[cfg(feature = "mod-suite")]
fn live_mod_sync_manifest_from_json(bytes: &[u8]) -> Result<lmb_sync::SyncManifest, String> {
    let entries = live_mod_json_rows(bytes, "entries", |index, entry| {
        let need = |key: &str| -> Result<String, String> {
            json::jstr(entry, key).ok_or_else(|| format!("entry {index}: missing string '{key}'"))
        };
        let size = entry
            .get("size_bytes")
            .map(J::num)
            .ok_or_else(|| format!("entry {index}: missing number 'size_bytes'"))?;
        Ok(lmb_sync::ModSyncEntry {
            mod_id: need("mod_id")?,
            file_name: need("file_name")?,
            download_url: need("download_url")?,
            sha256_hex: need("sha256_hex")?,
            size_bytes: if size >= 0.0 && size.is_finite() {
                size as u64
            } else {
                return Err(format!("entry {index}: 'size_bytes' out of range"));
            },
        })
    })?;
    Ok(lmb_sync::SyncManifest { entries })
}

#[cfg(feature = "mod-suite")]
fn live_mod_json_strings<C: FromIterator<String>>(row: &J, key: &str) -> Option<C> {
    json::jarr(row, key).map(|arr| arr.iter().map(|v| v.str().to_string()).collect())
}

/// Parse the progression-rules JSON described at `load_progression_json`.
#[cfg(feature = "mod-suite")]
fn live_mod_progression_rules_from_json(bytes: &[u8]) -> Result<Vec<ProgressionRule>, String> {
    live_mod_json_rows(bytes, "rules", |index, rule| {
        let id = json::jstr(rule, "id").ok_or_else(|| format!("rule {index}: missing 'id'"))?;
        let dimension = json::jstr(rule, "dimension")
            .ok_or_else(|| format!("rule {index} ({id}): missing 'dimension'"))?;
        let biomes = live_mod_json_strings(rule, "biomes")
            .ok_or_else(|| format!("rule {index} ({id}): missing 'biomes' array"))?;
        let requires = live_mod_json_strings(rule, "requires").unwrap_or_default();
        let effects = json::jarr(rule, "effects")
            .map(|arr| {
                arr.iter()
                    .filter_map(|e| {
                        let [id, duration, amplifier] = e.as_arr()?.as_slice() else {
                            return None;
                        };
                        Some((
                            id.str().to_string(),
                            duration.num() as u32,
                            amplifier.num().clamp(0.0, 255.0) as u8,
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let periodic_damage = json::jarr(rule, "damage").and_then(|parts| {
            let [hearts, interval] = parts.as_slice() else {
                return None;
            };
            let hearts = hearts.num() as f32;
            let interval = interval.num();
            (hearts.is_finite() && interval >= 1.0).then_some((hearts, interval as u32))
        });
        Ok(ProgressionRule {
            id,
            dimension,
            target_biomes: biomes,
            required_advancements: requires,
            block_break_protected: json::jbool(rule, "block_break", true),
            block_place_protected: json::jbool(rule, "block_place", true),
            enforce_effects: effects,
            periodic_damage,
        })
    })
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

#[cfg(all(test, feature = "mod-suite"))]
use crate::block_registry::LIVE_CREATE_TEST_GATE;
#[cfg(feature = "mod-suite")]
use crate::block_registry::{live_create_enabled, live_create_item_table, LIVE_CREATE_ITEM_BASE};

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
    live_create_item_entry(item).map(|(n, _)| n.as_str())
}

/// The block id a `create:*` item places, or `None` for the wrench.
#[cfg(feature = "mod-suite")]
fn live_create_item_block_name(item: i64) -> Option<&'static str> {
    live_create_item_entry(item)?.1.as_deref()
}

#[cfg(feature = "mod-suite")]
fn live_create_item_entry(item: i64) -> Option<&'static (String, Option<String>)> {
    let idx = usize::try_from(item.checked_sub(LIVE_CREATE_ITEM_BASE)?).ok()?;
    live_create_item_table().get(idx)
}

/// The armed Create block table for name→state lookups: the
/// `BReg`-attached registry when vanilla assets are present, else a
/// cached `core_create_registry` — the table is deterministic, so both
/// agree on every state id.
#[cfg(feature = "mod-suite")]
fn live_create_block_registry() -> Option<&'static create_adapter::blockstates::CreateBlockRegistry>
{
    if !live_create_enabled() {
        return None;
    }
    if let Some(reg) = breg().and_then(|reg| reg.create_registry()) {
        return Some(reg);
    }
    static FALLBACK: OnceLock<create_adapter::blockstates::CreateBlockRegistry> = OnceLock::new();
    Some(FALLBACK.get_or_init(|| {
        create_adapter::blockstates::core_create_registry(
            create_adapter::blockstates::DEFAULT_BASE_STATE_ID,
        )
    }))
}

/// `(block_name, partial_props)` → runtime state id across every live mod
/// block source: the armed Create registry (alias ids included —
/// `create:mechanical_belt` → `create:belt`) first, then the boot
/// [`UnifiedModRegistryPlan`] `MEGU_MODS_DIR` JAR block table — so
/// `/fill`, `/setblock`, and mod block placement resolve EVERY
/// data-driven JAR block, not only the hardcoded Create catalog. `None`
/// for unknown blocks or invalid properties — the same fail-closed
/// contract `BReg::checked_state` keeps.
#[cfg(feature = "mod-suite")]
fn live_mod_state_for_name(name: &str, properties: &[(String, String)]) -> Option<u32> {
    if let Some(create_reg) = live_create_block_registry() {
        if let Some(mut st) = create_reg.default_state(name) {
            for (key, value) in properties {
                st = create_reg.with_property(st, key, value)?;
            }
            return Some(st);
        }
    }
    live_mod_with_registry_plan(|plan| plan.checked_block_state(name, properties))
}

/// Reverse of [`live_mod_state_for_name`]: the canonical block id a live
/// plan state resolves to, or `None` for vanilla/Create-catalog ids.
#[cfg(feature = "mod-suite")]
fn live_mod_name_for_state(st: u32) -> Option<String> {
    live_mod_with_state(st, |entry| entry.block_id.clone())
}

/// Canonical `name[k=v]` spelling for a plan state — the MCF2 sidecar's
/// JAR-table counterpart of `CreateBlockRegistry::encode_canonical`.
#[cfg(feature = "mod-suite")]
fn live_mod_state_canonical(st: u32) -> Option<String> {
    live_mod_with_state(st, |entry| {
        mod_adapter_core::blockstates::format_canonical(&entry.block_id, &entry.properties)
    })
}

#[cfg(feature = "mod-suite")]
fn live_mod_with_state<R>(
    st: u32,
    convert: impl FnOnce(&DynamicBlockStateEntry) -> R,
) -> Option<R> {
    live_mod_with_registry_plan(|plan| plan.resolve_block_state(st).map(convert))
}

/// `name[k=v,...]` argument form of [`live_mod_state_for_name`], shared by
/// `/fill`/`/clone` (`modification_block_state`), `/setblock`
/// (`state_from_arg`), and the MCF2 sidecar decoder. Only namespaced
/// non-`minecraft:` names consult the live mod tables so vanilla
/// spellings keep their existing resolution order untouched.
#[cfg(feature = "mod-suite")]
fn live_mod_state_for_arg(arg: &str) -> Option<u32> {
    let (name, props) = match arg.split_once('[') {
        Some((n, rest)) => {
            let inner = rest.strip_suffix(']')?;
            let mut pairs = Vec::new();
            for part in inner.split(',') {
                let (k, v) = part.split_once('=')?;
                pairs.push((k.trim().to_string(), v.trim().to_string()));
            }
            (n.trim(), pairs)
        }
        None => (arg.trim(), Vec::new()),
    };
    if !name.contains(':') || name.starts_with("minecraft:") {
        return None;
    }
    live_mod_state_for_name(name, &props)
}

/// Non-mod-suite stub: no live mod registries exist, so modded names
/// never resolve.
#[cfg(not(feature = "mod-suite"))]
#[inline(always)]
fn live_mod_state_for_arg(_arg: &str) -> Option<u32> {
    None
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
        matches!(
            reg.name_of(st),
            "minecraft:water" | "minecraft:flowing_water"
        )
    })
}

/// `create:pressing` — a powered `create:mechanical_press` two blocks above
/// a depot or basin. `(&inputs, output)` accepts `#tag` spellings resolved
/// against the shared cross-mod tag index: exact ids keep stock behavior
/// with no mods loaded, while the conventional `c:`/`forge:` subtag lines
/// let any mod ingot filed under `ingots/*` press like its vanilla
/// counterpart.
#[cfg(feature = "mod-suite")]
const LIVE_CREATE_PRESSING: &[(&[&str], &str)] = &[
    (&["minecraft:iron_ingot"], "create:iron_sheet"),
    (&["minecraft:gold_ingot"], "create:gold_sheet"),
    (&["minecraft:copper_ingot"], "create:copper_sheet"),
    (&["create:brass_ingot"], "create:brass_sheet"),
    (&["#c:ingots/iron"], "create:iron_sheet"),
    (&["#c:ingots/gold"], "create:gold_sheet"),
    (&["#c:ingots/copper"], "create:copper_sheet"),
    (&["#c:ingots/brass"], "create:brass_sheet"),
];

/// `create:mixing` — a powered `create:mechanical_mixer` two blocks above a
/// basin. Same exact-id + `#tag` contract as [`LIVE_CREATE_PRESSING`].
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
    (
        &["minecraft:andesite", "#c:nuggets/iron"],
        "create:andesite_alloy",
    ),
];

/// Processing duration in ticks at `REFERENCE_RPM` — progress accrues in
/// rpm·tick units so a faster shaft finishes proportionally sooner.
#[cfg(feature = "mod-suite")]
const LIVE_CREATE_PROCESS_TICKS: u32 = 40;

/// One processing ingredient: an exact item id, or a `#tag` membership
/// probe resolved live against the shared cross-mod tag index so
/// `#c:ingots/iron` and `#forge:ingots/iron` (and a mod JAR's nested
/// equivalents) match uniformly.
#[cfg(feature = "mod-suite")]
#[derive(Clone, Debug)]
enum LiveCreateIngredient {
    Item(u32),
    Tag(String),
}

/// One resolved live processing recipe. `id` keeps pressing and mixing
/// key spaces disjoint for the `active` recipe pin on [`LiveCreateHeld`].
#[cfg(feature = "mod-suite")]
#[derive(Clone, Debug)]
struct LiveCreateRecipe {
    id: u32,
    base_ticks: u32,
    inputs: Vec<(LiveCreateIngredient, u32)>,
    outputs: Vec<(u32, u32)>,
}

/// Resolve the shared `(&inputs, output)` table form into
/// [`LiveCreateRecipe`]s: `#`-prefixed inputs become tag probes (they
/// never fail resolution — an absent tag simply matches nothing), plain
/// names resolve through the live item tables exactly like before.
#[cfg(feature = "mod-suite")]
fn live_create_recipes(table: &[(&[&str], &str)], id_base: u32) -> Vec<LiveCreateRecipe> {
    table
        .iter()
        .enumerate()
        .filter_map(|(idx, &(inputs, output))| {
            let inputs = inputs
                .iter()
                .map(|&name| {
                    let ingredient = match name.strip_prefix('#') {
                        Some(tag) => LiveCreateIngredient::Tag(tag.to_string()),
                        None => {
                            LiveCreateIngredient::Item(u32::try_from(name_to_item(name)?).ok()?)
                        }
                    };
                    Some((ingredient, 1u32))
                })
                .collect::<Option<Vec<_>>>()?;
            let out = u32::try_from(name_to_item(output)?).ok()?;
            Some(LiveCreateRecipe {
                id: id_base + idx as u32,
                base_ticks: LIVE_CREATE_PROCESS_TICKS,
                inputs,
                outputs: vec![(out, 1)],
            })
        })
        .collect()
}

/// `true` when the tracked item satisfies `ingredient`: exact ids compare
/// numerically; `#tag` inputs probe the shared unified index through the
/// live item name so a mod JAR's `#c:`/`#forge:` table decides.
#[cfg(feature = "mod-suite")]
fn live_create_ingredient_matches(ingredient: &LiveCreateIngredient, item: i64) -> bool {
    match ingredient {
        LiveCreateIngredient::Item(id) => u32::try_from(item).ok() == Some(*id),
        LiveCreateIngredient::Tag(tag) => item_to_name(item)
            .is_some_and(|name| live_unified_tag_matches(TagDomain::Item, tag, name)),
    }
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

/// The item-transport contract one tracked Create block carries through
/// the live `SleepingTransport` layer. Belts, chutes and funnels move
/// items between cells; depots and basins are the terminal slots the rest
/// of the runtime (`held`, pressing/mixing) already understands.
#[cfg(feature = "mod-suite")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LiveDuctKind {
    Belt,
    Chute,
    Funnel,
    BeltFunnel,
    Depot,
    Basin,
}

#[cfg(feature = "mod-suite")]
impl LiveDuctKind {
    /// `SleepingTransport` machine capacity for this duct — input and
    /// output FIFOs each hold at most this many item ids. Depot mirrors
    /// its single display slot; basin its nine.
    fn capacity(self) -> u32 {
        match self {
            Self::Belt => 1,
            Self::Chute => 4,
            Self::Funnel | Self::BeltFunnel | Self::Depot => 1,
            Self::Basin => 9,
        }
    }

    fn uses_held_slots(self) -> bool {
        matches!(self, Self::Depot | Self::Basin)
    }

    /// Capacity of the `held` slot overlay — only depot/basin own one.
    fn held_capacity(self) -> usize {
        match self {
            Self::Basin => 9,
            _ => 1,
        }
    }
}

/// Map a Create block id onto its [`LiveDuctKind`], or `None` for blocks
/// that carry no item transport (kinetics, casings, processors).
#[cfg(feature = "mod-suite")]
fn live_duct_kind(block_id: &str) -> Option<LiveDuctKind> {
    Some(match block_id {
        "create:belt" => LiveDuctKind::Belt,
        "create:chute" => LiveDuctKind::Chute,
        "create:andesite_funnel" | "create:brass_funnel" => LiveDuctKind::Funnel,
        "create:andesite_belt_funnel" | "create:brass_belt_funnel" => LiveDuctKind::BeltFunnel,
        "create:depot" => LiveDuctKind::Depot,
        "create:basin" => LiveDuctKind::Basin,
        _ => return None,
    })
}

/// `(dx, dy, dz)` for a `facing` property value — all six directions so
/// chutes, funnels and belts share one mapping.
#[cfg(feature = "mod-suite")]
fn live_facing_offset(facing: &str) -> Option<(i32, i32, i32)> {
    RsDir::from_property(facing).map(RsDir::offset)
}

/// Live `CreateFactory` bound to the server world. `placed` mirrors every
/// Create block currently in the world (the authoritative set for
/// persistence); `dormant` marks the subset — water wheels with no adjacent
/// water — that are placed but intentionally NOT registered in the kinetic
/// graph so they contribute no RPM until flow arrives. `held` is the
/// depot/basin item slot state persisted beside `placed`; `duct_machines`
/// maps every item-transport cell (belt, chute, funnels, depot, basin) onto
/// its lazily-registered `SleepingTransport` machine id.
#[cfg(feature = "mod-suite")]
struct LiveCreateRuntime {
    factory: create_adapter::factory::CreateFactory,
    placed: std::collections::BTreeMap<create_adapter::factory::DimBlockPos, u32>,
    dormant: std::collections::BTreeSet<create_adapter::factory::DimBlockPos>,
    held: std::collections::BTreeMap<create_adapter::factory::DimBlockPos, LiveCreateHeld>,
    duct_machines: std::collections::BTreeMap<create_adapter::factory::DimBlockPos, u32>,
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
        duct_machines: std::collections::BTreeMap::new(),
        next_machine_id: 1,
        pressing: live_create_recipes(LIVE_CREATE_PRESSING, 0),
        mixing: live_create_recipes(LIVE_CREATE_MIXING, 0x100),
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
    let mut guard = live_create_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
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
            // Unlink the orphaned machine so upstream ducts stop routing
            // into a cell that no longer accepts items.
            if let Some(&id) = self.duct_machines.get(&pos) {
                let _ = self.factory.link_belt(pos, id, None);
            }
            self.placed.remove(&pos);
            self.dormant.remove(&pos);
            self.held.remove(&pos);
            self.duct_machines.remove(&pos);
            let _ = self.factory.remove_block(pos);
            // The cell exposed no Create capability anymore — drop any
            // bus endpoints it carried.
            self.cap_sync_cell(pos);
            return;
        }
        // The block keeps its cell: drop slot/machine state the new state
        // can no longer own (e.g. a depot swapped for a shaft). Every duct
        // keeps its lazily-registered machine; only depot/basin own `held`
        // slots.
        match self
            .factory
            .registry()
            .block_def_for_state(st)
            .map(|def| def.id.as_str())
        {
            Some("create:depot" | "create:basin") => {}
            Some(id) if live_duct_kind(id).is_some() => {
                self.held.remove(&pos);
            }
            _ => {
                self.held.remove(&pos);
                if let Some(&id) = self.duct_machines.get(&pos) {
                    let _ = self.factory.link_belt(pos, id, None);
                }
                self.duct_machines.remove(&pos);
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
            // Every duct cell owns a transport machine from placement on:
            // links are only meaningful while the target machine exists,
            // and funnels need their staging input registered before their
            // pull cycle can run.
            if self.duct_kind_at(pos).is_some() {
                let _ = self.duct_machine(pos);
            }
        } else {
            let _ = self.factory.remove_block(pos);
            self.placed.insert(pos, st);
            self.dormant.insert(pos);
        }
        // Register/drop the cell's capability-bus endpoints to match the
        // new block id — `cap_sync_cell` no-ops when nothing changed, so
        // facing/powered flips keep stored cargo.
        self.cap_sync_cell(pos);
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
        self.duct_machines.clear();
        self.next_machine_id = 1;
        // Rebuilt runtime: drop every bus endpoint the previous graph
        // owned; each `sync_cell` below re-registers the restored cells.
        let stale: Vec<FactoryPos> = {
            let mut state = live_cap_state().lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut state.create_cells)
                .into_keys()
                .collect()
        };
        if !stale.is_empty() {
            let mut bus = live_cap_bus_guard();
            for pos in stale {
                live_cap_remove_endpoints(&mut bus, &live_cap_coord_of(pos));
            }
        }
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
            let held = LiveCreateHeld {
                items: items.into_iter().take(cap).collect(),
                ..Default::default()
            };
            self.held.insert(pos, held);
            // AUTO-OPT: a restored machine starts awake — its inputs just
            // (re)appeared, so the recipe scan runs next poll.
            live_auto_machine_wake_positions(&[pos]);
        }
    }

    /// Which `SleepingTransport` machine carries the duct cell at `pos` —
    /// `None` when the cell holds no duct.
    fn duct_kind_at(&self, pos: FactoryPos) -> Option<LiveDuctKind> {
        let st = *self.placed.get(&pos)?;
        let def = self.factory.registry().block_def_for_state(st)?;
        live_duct_kind(def.id.as_str())
    }

    fn insert_duct_item(&mut self, pos: FactoryPos, kind: LiveDuctKind, item: i64) -> Option<()> {
        if kind.uses_held_slots() {
            if self
                .held
                .get(&pos)
                .is_some_and(|h| h.items.len() >= kind.held_capacity())
            {
                return None;
            }
            let slot = self.held.entry(pos).or_default();
            slot.items.push(item);
            slot.progress = 0.0;
            slot.active = None;
        } else {
            let machine = self.duct_machine(pos)?;
            self.factory
                .insert_belt_item(pos, machine, u32::try_from(item).ok()?)
                .ok()?;
        }
        Some(())
    }

    /// The `MachineState` of the registered duct machine at `pos`, when the
    /// machine exists.
    fn duct_machine_state(
        &self,
        pos: FactoryPos,
    ) -> Option<mod_offload::sleeping_transport::MachineState> {
        let &id = self.duct_machines.get(&pos)?;
        self.factory
            .scheduler()
            .region(pos.region_key())?
            .transport()
            .machine(id)
    }

    /// Lazily register the `SleepingTransport` machine behind a duct block
    /// and return its region-local id. Belts scale their traversal delay
    /// inversely with the solved RPM (floor 8 rpm -> 8 ticks); every other
    /// duct moves items on a fixed short cadence.
    fn duct_machine(&mut self, pos: FactoryPos) -> Option<u32> {
        if let Some(&id) = self.duct_machines.get(&pos) {
            return Some(id);
        }
        let kind = self.duct_kind_at(pos)?;
        let delay = if kind == LiveDuctKind::Belt {
            let rpm = self
                .factory
                .kinetic_solution(pos)
                .map(|sol| sol.speed.abs())
                .unwrap_or(0.0);
            // The fixed-point belt kernel: one block of travel is
            // `SUB_UNITS_PER_BLOCK` at `rpm_to_belt_step(rpm)` per tick, so
            // the delay is `256 / step == 64 / rpm` computed in integers —
            // the same conversion `CreateBeltLine` runs at runtime.
            let step = create_adapter::factorio_sim::rpm_to_belt_step(rpm.max(8.0).round() as i32)
                .max(1) as u64;
            let units = u64::from(create_adapter::factorio_sim::SUB_UNITS_PER_BLOCK);
            ((units * 2 + step) / (step * 2)).clamp(1, 64)
        } else {
            2
        };
        let id = self.next_machine_id;
        self.next_machine_id = self.next_machine_id.saturating_add(1).max(1);
        self.factory
            .add_belt_segment(pos, id, kind.capacity(), delay)
            .ok()?;
        self.duct_machines.insert(pos, id);
        Some(id)
    }

    /// The cell `pos`'s duct pushes items toward under the live topology,
    /// plus the direction the push travels — `None` for sinks, blocked
    /// ducts and powered-off funnels. Unlike [`Self::duct_downstream_pos`]
    /// the target is NOT required to be a duct cell: this is the
    /// capability-bus counterpart, so a chute can feed a cooking pot or a
    /// foreign mod's storage block.
    fn cap_push_target(&self, pos: FactoryPos, st: u32) -> Option<(FactoryPos, (i32, i32, i32))> {
        let def = self.factory.registry().block_def_for_state(st)?;
        let kind = live_duct_kind(def.id.as_str())?;
        let entry = self.factory.registry().state(st)?;
        if entry.property("powered") == Some("true") {
            return None;
        }
        let facing = entry.property("facing").and_then(live_facing_offset);
        let extracting = entry.property("extracting") == Some("true");
        Some(match kind {
            // Belts run items toward their horizontal `facing` direction.
            LiveDuctKind::Belt => {
                let d = facing?;
                (pos.offset(d.0, d.1, d.2), d)
            }
            // Chutes are gravity ducts: straight down into the cell below.
            LiveDuctKind::Chute => (pos.offset(0, -1, 0), (0, -1, 0)),
            // Insert-mode funnels push out of the mouth (`facing`); in
            // extracting mode staged items ride back into the attached
            // container (the cell behind the mouth).
            LiveDuctKind::Funnel => {
                let d = facing?;
                if extracting {
                    (pos.offset(-d.0, -d.1, -d.2), (-d.0, -d.1, -d.2))
                } else {
                    (pos.offset(d.0, d.1, d.2), d)
                }
            }
            // Belt funnels sit one cell over their belt. Insert mode drops
            // onto the belt below; extracting mode serves the container the
            // mouth points at.
            LiveDuctKind::BeltFunnel => {
                if extracting {
                    let d = facing?;
                    (pos.offset(d.0, d.1, d.2), d)
                } else {
                    (pos.offset(0, -1, 0), (0, -1, 0))
                }
            }
            LiveDuctKind::Depot | LiveDuctKind::Basin => return None,
        })
    }

    /// The cell `pos`'s duct pushes items toward under the live topology,
    /// or `None` for sinks, blocked ducts and powered-off funnels. The
    /// target must itself be a duct cell — the machine it owns is what the
    /// `SleepingTransport` link actually points at.
    fn duct_downstream_pos(&self, pos: FactoryPos, st: u32) -> Option<FactoryPos> {
        let (target, _) = self.cap_push_target(pos, st)?;
        self.duct_kind_at(target)?;
        Some(target)
    }

    /// Keep the shared capability bus in step with `pos`: register the
    /// endpoints the placed block's id declares (first registration or
    /// capability-shape change only — a mere facing/powered flip must not
    /// drop stored cargo), unregister when the cell stops exposing any.
    fn cap_sync_cell(&mut self, pos: FactoryPos) {
        let block_id = self
            .placed
            .get(&pos)
            .and_then(|&st| self.factory.registry().block_def_for_state(st))
            .map(|def| def.id.clone());
        let endpoints = block_id
            .as_deref()
            .map(create_adapter::factory::create_capability_endpoints_for)
            .filter(|eps| !eps.is_empty());
        let coord = live_cap_coord_of(pos);
        // Lock order is global: cap_state -> bus.
        let mut state = live_cap_state().lock().unwrap_or_else(|e| e.into_inner());
        // A Create endpoint always wins the cell over a stale profile
        // registration — the profile row can go, the bus handlers below
        // get replaced in place.
        state.profile_cells.remove(&coord);
        let tracked = state.create_cells.get(&pos).map(String::as_str);
        if tracked == block_id.as_deref().filter(|_| endpoints.is_some()) {
            return;
        }
        let mut bus = live_cap_bus_guard();
        match (endpoints, block_id) {
            (Some(eps), Some(id)) => {
                eps.register_into(&mut bus, &coord);
                state.create_cells.insert(pos, id);
            }
            _ => {
                state.create_cells.remove(&pos);
                live_cap_remove_endpoints(&mut bus, &coord);
            }
        }
    }

    /// One cross-mod transfer pass on the shared bus: every armed duct
    /// pushes up to [`LIVE_CAP_STEP_ITEMS`] items toward its live
    /// downstream cell when that cell hosts a foreign bus item endpoint
    /// (duct-to-duct flow stays on `SleepingTransport`), and Create fluid
    /// pipes/pumps push up to [`LIVE_CAP_STEP_FLUID_MB`] toward adjacent
    /// fluid endpoints. The bus's simulate-then-commit keeps every hop
    /// atomic. Returns the `BlockCoord`s that received items so callers
    /// can re-arm cooking-pot deadlines outside the bus lock.
    fn cap_bus_step(&mut self) -> Vec<BlockCoord> {
        let cells: Vec<(FactoryPos, u32)> = {
            let state = live_cap_state().lock().unwrap_or_else(|e| e.into_inner());
            state
                .create_cells
                .keys()
                .filter_map(|pos| self.placed.get(pos).map(|&st| (*pos, st)))
                .collect()
        };
        if cells.is_empty() {
            return Vec::new();
        }
        let mut fed = Vec::new();
        let mut bus = live_cap_bus_guard();
        for (pos, st) in cells {
            let from = live_cap_coord_of(pos);
            if let Some((dst, push)) = self.cap_push_target(pos, st) {
                // Only push into foreign endpoints — a duct target's items
                // ride the `SleepingTransport` link, not the bus.
                if self.duct_kind_at(dst).is_none() {
                    let to = live_cap_coord_of(dst);
                    if bus.item_endpoint(&to).is_some() {
                        let to_face = live_cap_face_of((-push.0, -push.1, -push.2));
                        let mut moved = 0u32;
                        // (a) Staged transport output first — items that
                        //     entered the duct through
                        //     `live_create_duct_insert` ride the machine
                        //     FIFO, not the slot store. A refused item
                        //     recirculates through the input FIFO instead
                        //     of vanishing.
                        if let Some(&machine) = self.duct_machines.get(&pos) {
                            while moved < LIVE_CAP_STEP_ITEMS {
                                let Some(item) = self
                                    .factory
                                    .scheduler_mut()
                                    .region_mut(pos.region_key())
                                    .extract_output(machine)
                                    .ok()
                                    .flatten()
                                else {
                                    break;
                                };
                                let name = live_cap_item_name(i64::from(item));
                                let stack = CapabilityItemStack::new(name, 1);
                                let refused = bus
                                    .item_endpoint_mut(&to)
                                    .map(|ep| ep.insert_item(Some(to_face), &stack, true).count)
                                    .unwrap_or(1);
                                if refused > 0 {
                                    let _ = self.factory.insert_belt_item(pos, machine, item);
                                    break;
                                }
                                if let Some(ep) = bus.item_endpoint_mut(&to) {
                                    let _ = ep.insert_item(Some(to_face), &stack, false);
                                }
                                moved += 1;
                            }
                        }
                        // (b) The endpoint slot store — component-tagged and
                        //     foreign-id stacks the transport machinery
                        //     cannot carry.
                        if moved < LIVE_CAP_STEP_ITEMS {
                            moved += bus.transfer_item_between(
                                &from,
                                Some(live_cap_face_of(push)),
                                &to,
                                Some(to_face),
                                LIVE_CAP_STEP_ITEMS - moved,
                                None,
                            );
                        }
                        if moved > 0 {
                            fed.push(to);
                        }
                    }
                }
            }
            let fluid_pusher = self
                .factory
                .registry()
                .block_def_for_state(st)
                .is_some_and(|def| {
                    matches!(
                        def.id.as_str(),
                        "create:fluid_pipe" | "create:mechanical_pump"
                    )
                });
            if fluid_pusher && bus.fluid_endpoint(&from).is_some() {
                for d in [
                    (0i32, -1i32, 0i32),
                    (0, 1, 0),
                    (0, 0, -1),
                    (0, 0, 1),
                    (-1, 0, 0),
                    (1, 0, 0),
                ] {
                    let to = live_cap_coord_of(pos.offset(d.0, d.1, d.2));
                    if bus.fluid_endpoint(&to).is_some() {
                        bus.transfer_fluid_between(
                            &from,
                            Some(live_cap_face_of(d)),
                            &to,
                            Some(live_cap_face_of((-d.0, -d.1, -d.2))),
                            LIVE_CAP_STEP_FLUID_MB,
                        );
                    }
                }
            }
        }
        fed
    }

    /// Re-point every registered duct machine's downstream at the machine
    /// owning its desired target cell. Links are recomputed each tick so
    /// belt-facing changes, fresh placements and removed cells all take
    /// effect without an explicit rewire pass; `set_downstream` is a no-op
    /// when the link already matches. Cross-region and contested targets
    /// (one upstream per machine) are skipped fail-soft — items stall in
    /// the source's output FIFO instead of vanishing.
    fn relink_ducts(&mut self) {
        if self.duct_machines.is_empty() {
            return;
        }
        let sources: Vec<FactoryPos> = self.duct_machines.keys().copied().collect();
        for pos in sources {
            let desired = self
                .placed
                .get(&pos)
                .copied()
                .and_then(|st| self.duct_downstream_pos(pos, st))
                .filter(|dst| dst.region_key() == pos.region_key())
                .and_then(|dst| self.duct_machine(dst));
            let Some(from) = self.duct_machines.get(&pos).copied() else {
                continue;
            };
            let _ = self.factory.link_belt(pos, from, desired);
        }
    }

    /// Settle pass run after each transport advance: depot/basin machine
    /// outputs drain into their `held` slot (capacity-bounded, so full
    /// slots backpressure the machine), then funnels pull one item each
    /// from their source cell into their staging input — the downstream
    /// link carries it onward from there.
    fn drain_ducts(&mut self) {
        if self.duct_machines.is_empty() {
            return;
        }
        let positions: Vec<FactoryPos> = self.duct_machines.keys().copied().collect();
        let mut fed: Vec<FactoryPos> = Vec::new();
        for pos in positions {
            let Some(kind) = self.duct_kind_at(pos) else {
                continue;
            };
            match kind {
                LiveDuctKind::Depot | LiveDuctKind::Basin => {
                    let cap = kind.held_capacity();
                    let Some(&machine) = self.duct_machines.get(&pos) else {
                        continue;
                    };
                    loop {
                        let filled = self.held.get(&pos).map_or(0, |h| h.items.len());
                        if filled >= cap {
                            break;
                        }
                        match self
                            .factory
                            .scheduler_mut()
                            .region_mut(pos.region_key())
                            .extract_output(machine)
                        {
                            Ok(Some(item)) => {
                                self.held
                                    .entry(pos)
                                    .or_default()
                                    .items
                                    .push(i64::from(item));
                                fed.push(pos);
                            }
                            _ => break,
                        }
                    }
                }
                LiveDuctKind::Funnel | LiveDuctKind::BeltFunnel => {
                    self.funnel_pull(pos, kind);
                }
                _ => {}
            }
        }
        // AUTO-OPT: a duct delivery changes a machine's inputs — the
        // recipe fingerprint moves, so its cadence resets to 1.
        live_auto_machine_wake_positions(&fed);
    }

    /// One funnel transfer cycle at `pos`: when the funnel's staging input
    /// is free, pull a single item out of its source cell — a depot/basin
    /// `held` slot or an upstream duct's output FIFO — and queue it into
    /// the funnel machine, whose downstream link completes the hop.
    /// Extracting funnels pull from the block the mouth faces (belt
    /// funnels: the belt below) toward the attached container; inserting
    /// funnels pull from the attached container (belt funnels: the faced
    /// one) toward the mouth / the belt below.
    fn funnel_pull(&mut self, pos: FactoryPos, kind: LiveDuctKind) {
        let Some(st) = self.placed.get(&pos).copied() else {
            return;
        };
        let Some(entry) = self.factory.registry().state(st) else {
            return;
        };
        if entry.property("powered") == Some("true") {
            return;
        }
        let extracting = entry.property("extracting") == Some("true");
        let Some(d) = entry.property("facing").and_then(live_facing_offset) else {
            return;
        };
        let (src, dst) = match kind {
            LiveDuctKind::Funnel => {
                let front = pos.offset(d.0, d.1, d.2);
                let back = pos.offset(-d.0, -d.1, -d.2);
                if extracting {
                    (front, back)
                } else {
                    (back, front)
                }
            }
            LiveDuctKind::BeltFunnel => {
                let belt = pos.offset(0, -1, 0);
                if self.duct_kind_at(belt) != Some(LiveDuctKind::Belt) {
                    return;
                }
                let served = pos.offset(d.0, d.1, d.2);
                if extracting {
                    (belt, served)
                } else {
                    (served, belt)
                }
            }
            _ => return,
        };
        // The destination must be a duct the staging machine can link into;
        // anything else means the funnel ejects into open air, which this
        // model leaves staged in its output FIFO for manual pickup.
        if self.duct_kind_at(dst).is_none() {
            return;
        }
        let Some(&funnel_id) = self.duct_machines.get(&pos) else {
            return;
        };
        if self
            .duct_machine_state(pos)
            .is_none_or(|m| m.input_items != 0)
        {
            return;
        }
        let src_kind = self.duct_kind_at(src);
        let item: Option<i64> = match src_kind {
            Some(LiveDuctKind::Depot | LiveDuctKind::Basin) => {
                self.held.get_mut(&src).and_then(|h| h.items.pop())
            }
            Some(_) => self
                .duct_machines
                .get(&src)
                .copied()
                .and_then(|id| {
                    self.factory
                        .scheduler_mut()
                        .region_mut(src.region_key())
                        .extract_output(id)
                        .ok()
                        .flatten()
                })
                .map(i64::from),
            None => None,
        };
        let Some(item) = item else {
            return;
        };
        // AUTO-OPT: a funnel pull emptied or shrank a source slot — the
        // recipe fingerprint moved, so the source's cadence resets.
        if matches!(src_kind, Some(LiveDuctKind::Depot | LiveDuctKind::Basin)) {
            live_auto_machine_wake_positions(&[src]);
            if self.held.get(&src).is_some_and(|h| h.items.is_empty()) {
                self.held.remove(&src);
            }
        }
        match u32::try_from(item) {
            Ok(item) => {
                let _ = self.factory.insert_belt_item(pos, funnel_id, item);
            }
            Err(_) => {
                // Transport item ids are u32; an out-of-range slot entry
                // returns to its source instead of silently disappearing.
                if matches!(src_kind, Some(LiveDuctKind::Depot | LiveDuctKind::Basin)) {
                    self.held.entry(src).or_default().items.push(item);
                }
            }
        }
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
        let tick = TICK_IDX.load(Ordering::Relaxed);
        let positions: Vec<FactoryPos> = self.held.keys().copied().collect();
        for pos in positions {
            // AUTO-OPT: an idle machine polls on the adaptive
            // `1 -> 2 -> 4 -> 8 -> 20` backoff ladder — a skipped poll
            // costs one backoff-table probe; inventory/structure writes
            // wake the entry back to cadence 1 immediately.
            if !live_auto_machine_should_tick(pos, tick) {
                continue;
            }
            let changed = self.tick_processing_cell(pos);
            if self.held.contains_key(&pos) {
                live_auto_machine_record(pos, changed);
            } else {
                live_auto_machine_remove(pos);
            }
        }
    }

    /// One granted processing poll at `pos`, reporting whether observable
    /// machine state changed (progress accrued, items consumed/produced,
    /// a match lost or gained). `false` lets the backoff ladder stretch
    /// the cell's cadence; `true` snaps it back to every-tick polling.
    fn tick_processing_cell(&mut self, pos: FactoryPos) -> bool {
        let Some(&st) = self.placed.get(&pos) else {
            self.held.remove(&pos);
            return false;
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
            return false;
        }
        let proc_pos = pos.offset(0, 2, 0);
        let Some(&pst) = self.placed.get(&proc_pos) else {
            return false;
        };
        let Some(pdef) = self.factory.registry().block_def_for_state(pst) else {
            return false;
        };
        let mixer = basin && pdef.id == "create:mechanical_mixer";
        if pdef.id != "create:mechanical_press" && !mixer {
            return false;
        }
        let Some(sol) = self.factory.kinetic_solution(proc_pos) else {
            return false;
        };
        if !matches!(
            sol.status,
            mod_offload::kinetic_graph::NetworkStatus::Running
        ) || sol.speed.abs() < 1e-6
        {
            return false;
        }
        let rpm = sol.speed.abs();
        let kind = if mixer {
            LIVE_OPT_MACHINE_MIXER
        } else {
            LIVE_OPT_MACHINE_PRESS
        };
        let recipes = if mixer { &self.mixing } else { &self.pressing };
        let Some(slot) = self.held.get_mut(&pos) else {
            return false;
        };
        // AUTO-OPT: the recipe scan is memoized per `(kind, items
        // fingerprint)` — an idle machine holding unmatched items runs
        // its matcher once, not once per granted poll.
        let fingerprint = live_fingerprint_held_items(&slot.items);
        let items = &slot.items;
        let Some(recipe_idx) = live_auto_recipe_lookup(kind, fingerprint, |tags| {
            let count_item = |need: &(LiveCreateIngredient, u32)| {
                items
                    .iter()
                    .filter(|item| live_create_ingredient_matches_tags(&need.0, **item, tags))
                    .count()
            };
            recipes
                .iter()
                .position(|r| {
                    r.inputs
                        .iter()
                        .all(|need| count_item(need) >= need.1 as usize)
                })
                .map(|idx| idx as u32)
        }) else {
            if slot.progress != 0.0 || slot.active.is_some() {
                slot.progress = 0.0;
                slot.active = None;
                return true;
            }
            return false;
        };
        let recipe = &recipes[recipe_idx as usize];
        let key = recipe.id as u16;
        if slot.active != Some(key) {
            slot.active = Some(key);
            slot.progress = 0.0;
        }
        slot.progress += rpm;
        let need =
            f64::from(recipe.base_ticks) * f64::from(mod_offload::recipe_processor::REFERENCE_RPM);
        if slot.progress < need {
            return true;
        }
        for (ingredient, amount) in &recipe.inputs {
            for _ in 0..*amount {
                if let Some(at) = slot
                    .items
                    .iter()
                    .position(|item| live_create_ingredient_matches(ingredient, *item))
                {
                    slot.items.remove(at);
                }
            }
        }
        for &(output, amount) in &recipe.outputs {
            for _ in 0..amount {
                slot.items.push(i64::from(output));
            }
        }
        slot.progress = 0.0;
        slot.active = None;
        true
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
    let water_edge = !was_mod && !is_mod && live_create_is_water(old) != live_create_is_water(st);
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
    // AUTO-OPT: a block write may (dis)arm the depot/basin below or the
    // processor two cells above — wake every machine this cell could feed
    // so a sleeping one resumes at cadence 1.
    if was_mod || is_mod {
        live_auto_machine_wake_positions(&[pos, pos.offset(0, -2, 0), pos.offset(0, 2, 0)]);
    }
    // XMOD1: generic plan blocks expose their declared profile endpoints
    // on the shared bus (and a cell that stopped being one drops them).
    if was_mod || is_mod {
        live_cap_sync_profile_endpoint(dim, x, y, z, st);
    }
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

/// Right-click on a tracked duct cell — `create:depot`, `create:belt`,
/// `create:basin`, `create:chute` or any `create:*funnel` — without the
/// wrench. A held item inserts one unit into the machine's item slot
/// (depot 1, basin 9, belt/chute/funnel machine inputs per
/// [`LiveDuctKind::capacity`]) and the caller consumes it like a
/// placement; an empty hand — or any sneak-click — pulls the stored or
/// processed item back through the vanilla `Inventory.add` pickup path.
/// Returns `false` when the click is not handled so the normal
/// placement/interaction flow still runs.
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
        let kind = rt.duct_kind_at(pos)?;
        if held_item != 0 && !sneaking {
            rt.insert_duct_item(pos, kind, held_item)?;
            return Some(LiveCreateInteract::Inserted);
        }
        let slot_kind = kind.uses_held_slots();
        // Retrieval: extract first, then `Inventory.add`; a full inventory
        // re-seats the item so a failed grab never deletes it.
        let item: i64 = if slot_kind {
            rt.held.get_mut(&pos)?.items.pop()?
        } else {
            let machine = *rt.duct_machines.get(&pos)?;
            rt.factory
                .scheduler_mut()
                .region_mut(pos.region_key())
                .extract_output(machine)
                .ok()?
                .map(i64::from)?
        };
        let mut inv = me.inv.lock().unwrap_or_else(|e| e.into_inner());
        let selected = *me.held.lock().unwrap_or_else(|e| e.into_inner());
        let creative = *me.gm.lock().unwrap_or_else(|e| e.into_inner()) == 1;
        let before = inv.clone();
        let mut source = (item, 1u8, Vec::new());
        let accepted = pickup_into_inventory(&mut inv, selected, creative, &mut source);
        if !accepted || source.1 != 0 {
            if slot_kind {
                if let Some(slot) = rt.held.get_mut(&pos) {
                    slot.items.push(item);
                }
            } else if let (Some(&machine), Ok(item)) =
                (rt.duct_machines.get(&pos), u32::try_from(item))
            {
                let _ = rt.factory.insert_belt_item(pos, machine, item);
            }
            return None;
        }
        if slot_kind && rt.held.get(&pos).is_some_and(|h| h.items.is_empty()) {
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
    // AUTO-OPT: an inserted or retrieved item moved the machine's recipe
    // fingerprint — reset its cadence so processing resumes at once.
    live_auto_machine_wake_positions(&[pos]);
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

/// Drop one `item` onto the duct cell at `(x, y, z)` — the shared
/// non-interaction entry point for thrown items, scripted feeds and the
/// schematic end-to-end path. Depot/basin land in their `held` slot;
/// every other duct queues into its `SleepingTransport` machine input so
/// links and funnel pulls carry it onward. `false` when the cell holds no
/// duct, is full, or the item id is out of range.
#[cfg(feature = "mod-suite")]
fn live_create_duct_insert(dim: DimId, x: i32, y: i32, z: i32, item: i64) -> bool {
    live_create_with(|rt| {
        let pos = FactoryPos::new(dim.raw() as i32, x, y, z);
        let kind = rt.duct_kind_at(pos)?;
        rt.insert_duct_item(pos, kind, item)?;
        if kind.uses_held_slots() {
            // A scripted feed into a held slot resets recipe polling cadence.
            live_auto_machine_wake_positions(&[pos]);
        }
        Some(())
    })
    .flatten()
    .is_some()
}

#[cfg(feature = "mod-suite")]
fn schematic_nbt_to_live_tag(value: &create_adapter::schematic::SchematicNbt) -> Option<Tag> {
    use create_adapter::schematic::SchematicNbt as S;
    match value {
        S::Byte(v) => Some(Tag::Byte(*v)),
        S::Short(v) => Some(Tag::Short(*v)),
        S::Int(v) => Some(Tag::Int(*v)),
        S::Long(v) => Some(Tag::Long(*v)),
        S::Float(v) => Some(Tag::Float(v.to_bits())),
        S::Double(v) => Some(Tag::Double(v.to_bits())),
        S::ByteArray(v) => Some(Tag::ByteArray(v.clone())),
        S::String(v) => Some(Tag::String(nbt::NbtString::text(v))),
        S::List(values) => {
            let converted: Vec<Tag> = values
                .iter()
                .map(schematic_nbt_to_live_tag)
                .collect::<Option<_>>()?;
            let element = converted.first().map(Tag::id).unwrap_or(0);
            Some(Tag::List(element, converted))
        }
        S::Compound(values) => {
            let mut out = std::collections::BTreeMap::new();
            for (key, value) in values {
                out.insert(nbt::NbtString::text(key), schematic_nbt_to_live_tag(value)?);
            }
            Some(Tag::Compound(out))
        }
        S::IntArray(v) => Some(Tag::IntArray(v.clone())),
        S::LongArray(v) => Some(Tag::LongArray(v.clone())),
    }
}

#[cfg(feature = "mod-suite")]
fn restore_schematic_block_entity(
    dim: DimId,
    pos: [i32; 3],
    state: u32,
    value: Option<&create_adapter::schematic::SchematicNbt>,
) {
    let Some(value) = value else { return };
    let Some(mut tag) = schematic_nbt_to_live_tag(value) else {
        return;
    };
    let Some(reg) = breg() else { return };
    let Some(id) = live_block_entity_id(reg.name_of(state)) else {
        return;
    };
    if let Tag::Compound(mut fields) = tag {
        for (key, value) in [("x", pos[0]), ("y", pos[1]), ("z", pos[2])] {
            block_entity_put(&mut fields, key, Tag::Int(value));
        }
        block_entity_put(&mut fields, "id", block_entity_string(id));
        tag = Tag::Compound(fields);
        let _ = restore_live_block_entity((dim, pos[0], pos[1], pos[2]), &tag);
    }
}

/// Place one `.nbt` structure — the format Structure Blocks write and
/// Create ships for its Ponder/showcase factories — at `origin` inside any
/// dimension. Each palette entry resolves once through `state_from_arg`
/// (canonical `name[k=v]` spelling), so vanilla, Create-catalog and
/// `MEGU_MODS_DIR` JAR blocks all land through one path; every write goes
/// through `set_block_if_dim`, which fires `live_create_on_block_set` and
/// thereby arms the Create runtime exactly like a player-driven edit.
/// Fails closed: a malformed or over-cap schematic rejects before any
/// world write.
#[cfg(feature = "mod-suite")]
fn live_place_nbt_schematic_in_dim(
    dim: DimId,
    origin: (i32, i32, i32),
    nbt_bytes: &[u8],
) -> Result<create_adapter::schematic::SchematicPlacementReport, String> {
    let structure =
        create_adapter::schematic::parse_nbt_schematic(nbt_bytes).map_err(|err| err.to_string())?;
    let plan = create_adapter::schematic::SchematicPlacementPlan::preflight(
        &structure,
        [origin.0, origin.1, origin.2],
        state_from_arg,
        |[x, y, z]| {
            if !(-30_000_000..30_000_000).contains(&x)
                || !(-30_000_000..30_000_000).contains(&z)
                || !(dim.min_y()..dim.min_y() + dim.height()).contains(&y)
            {
                return false;
            }
            let existing = block_at_dim(dim, x, y, z);
            breg().map_or(existing == 0, |reg| reg.is_air(existing))
        },
    )
    .map_err(|err| err.to_string())?;
    let mut placed_positions: Vec<FactoryPos> = Vec::new();
    let report = plan.apply(|pos, state, entity_nbt| {
        if set_block_if_dim(dim, pos[0], pos[1], pos[2], state, None) {
            restore_schematic_block_entity(dim, pos, state, entity_nbt);
            placed_positions.push(FactoryPos::new(dim.raw() as i32, pos[0], pos[1], pos[2]));
        }
    });
    let mut report = report;
    report.kinetic_nodes_armed = live_create_with(|rt| {
        placed_positions
            .iter()
            .filter(|pos| rt.placed.contains_key(pos) && !rt.dormant.contains(*pos))
            .count()
    })
    .unwrap_or(0);
    Ok(report)
}

/// Wire-facing `.nbt` schematic placement: `dim` is the raw dimension
/// slot ([`DimId::raw`]). Same fail-closed semantics as
/// [`live_place_nbt_schematic_in_dim`].
#[cfg(feature = "mod-suite")]
pub fn live_place_nbt_schematic(
    dim: u32,
    origin: (i32, i32, i32),
    nbt_bytes: &[u8],
) -> Result<create_adapter::schematic::SchematicPlacementReport, String> {
    live_place_nbt_schematic_in_dim(DimId::from_raw(dim), origin, nbt_bytes)
}

/// Per-region per-tick wall-clock budget handed to
/// `CreateFactory::step_tick`, shed by the server mspt governor
/// (`live_server_budget_scale`) under load — idle regions never consume it
/// at all thanks to the factory's sleep bookkeeping.
#[cfg(feature = "mod-suite")]
const LIVE_CREATE_REGION_BUDGET_US: u32 = 2_000;

/// Legacy single-frame encoding retained for small tests/tools. Runtime
/// broadcast uses the chunked dimension-scoped frames instead.
#[cfg(all(test, feature = "mod-suite"))]
fn live_create_tick_body() -> Option<Vec<u8>> {
    live_create_tick_tags().map(|tags| {
        live_mod_payload_body(
            create_adapter::neoforge_wire::CHANNEL_CREATE_KINETIC_SYNC,
            &create_adapter::neoforge_wire::encode_kinetic_update_batch(&tags),
        )
    })
}

#[cfg(feature = "mod-suite")]
fn live_create_tick_tags() -> Option<Vec<create_adapter::factory::KineticUpdateTag>> {
    if !live_create_enabled() {
        return None;
    }
    let tick = TICK_IDX.load(Ordering::Relaxed);
    let budget_us = ((f64::from(LIVE_CREATE_REGION_BUDGET_US)
        * f64::from(live_server_budget_scale()))
    .max(1.0)) as u64;
    live_create_with(|rt| {
        rt.tick_processing();
        rt.relink_ducts();
        let _ = rt.factory.step_tick(tick, 1_024, Some(budget_us));
        rt.drain_ducts();
        let mut drained = Vec::new();
        rt.factory.drain_update_batch_into(&mut drained);
        if drained.is_empty() {
            return None;
        }
        let tags: Vec<create_adapter::factory::KineticUpdateTag> =
            drained.into_iter().map(|(_, tag)| tag).collect();
        rt.kinetic_batches_emitted += 1;
        Some(tags)
    })
    .flatten()
}

/// Per-game-tick stage (`run("live_create", live_create_tick)`): broadcast
/// the drained kinetic batch to every connected client over Play S2C
/// `0x19` on channel `create:kinetic_sync_v1`. The factory lock is released
/// before any socket is touched.
#[cfg(feature = "mod-suite")]
fn live_create_tick() {
    let Some(tags) = live_create_tick_tags() else {
        return;
    };
    let generation = TICK_IDX.load(Ordering::Relaxed);
    let frames = create_adapter::neoforge_wire::encode_kinetic_batches(&tags, generation);
    for client in client_snapshot() {
        let dim = (*client.dim.lock().unwrap_or_else(|e| e.into_inner())).raw() as i32;
        let mut socket = client.sock.lock().unwrap_or_else(|e| e.into_inner());
        live_create_send_kinetic_frames(&mut socket, &frames, Some(dim));
    }
}

/// Full kinetic snapshot packet for a joining client, sent right after
/// `live_mod_sync_on_join` so late joiners see spinning wheels/shafts
/// without waiting for the next delta.
#[cfg(feature = "mod-suite")]
fn live_create_sync_on_join(s: &mut TcpStream) {
    // MRS1 is a strict prerequisite for interpreting dynamic Create state ids
    // in the kinetic snapshot. `play_protocol` also emits the registry sync in
    // its ordered join sequence; sending it here as well keeps this helper
    // safe when tests or future call sites invoke it directly.
    live_mod_registry_sync_on_join(s);
    let Some(tags) = live_create_with(|rt| {
        let tags = rt.snapshot_tags();
        (!tags.is_empty()).then_some(tags)
    })
    .flatten() else {
        return;
    };
    let generation = TICK_IDX.load(Ordering::Relaxed);
    let frames = create_adapter::neoforge_wire::encode_kinetic_batches(&tags, generation);
    let peer = s.peer_addr().ok();
    let client_dim = peer.and_then(|peer| {
        client_snapshot().into_iter().find_map(|client| {
            let addr = client
                .sock
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .peer_addr()
                .ok()?;
            (addr == peer)
                .then(|| (*client.dim.lock().unwrap_or_else(|e| e.into_inner())).raw() as i32)
        })
    });
    live_create_send_kinetic_frames(s, &frames, client_dim);
}

#[cfg(feature = "mod-suite")]
fn live_create_send_kinetic_frames(s: &mut TcpStream, frames: &[Vec<u8>], dim: Option<i32>) {
    use create_adapter::neoforge_wire::CHANNEL_CREATE_KINETIC_SYNC;

    for frame in frames {
        // Kinetic batch header: magic, version, then the dimension ID.
        if dim.is_some_and(|dim| frame.get(5..9) != Some(&dim.to_be_bytes())) {
            continue;
        }
        let _ = live_mod_send_payload(s, CHANNEL_CREATE_KINETIC_SYNC, frame);
    }
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

/// MCF2 canonical spelling for a tracked state: the Create registry's
/// `encode_canonical` first, then the [`UnifiedModRegistryPlan`] JAR block
/// table (a `create:*` state the core catalog does not own still gets a
/// name), and finally a deterministic non-empty fallback — a state that
/// resolves nowhere must never serialize as `""`, which previously made
/// the whole sidecar fail closed on reload.
#[cfg(feature = "mod-suite")]
fn live_create_encode_state(
    registry: &create_adapter::blockstates::CreateBlockRegistry,
    st: u32,
) -> String {
    if let Some(canon) = registry.encode_canonical(st) {
        return canon.to_string();
    }
    if let Some(canon) = live_mod_state_canonical(st) {
        return canon;
    }
    format!("megu:unresolved_block_state[{st}]")
}

/// `megu:unresolved_block_state[<id>]` — the non-empty fallback
/// [`live_create_encode_state`] writes for a state no registry names.
/// Decoding hands the raw id back so `load_entries` drops the cell
/// through the ordinary `sync_cell` gate instead of rejecting the whole
/// sidecar.
#[cfg(feature = "mod-suite")]
fn live_create_unresolved_state(canon: &str) -> Option<u32> {
    canon
        .strip_prefix("megu:unresolved_block_state[")
        .and_then(|rest| rest.strip_suffix(']'))
        .and_then(|num| num.parse::<u32>().ok())
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
            let canon = live_create_encode_state(&registry, *st);
            write_str(&mut out, &canon);
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
type LiveCreateSidecarEntry = (FactoryPos, u32, Vec<i64>);

#[cfg(feature = "mod-suite")]
fn live_create_decode_sidecar(
    raw: &[u8],
) -> Result<Vec<LiveCreateSidecarEntry>, &'static str> {
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
    // A crafted `count` (up to 1_000_000) must not become a ~44 MB
    // pre-allocation before the entry bytes are validated: every entry costs
    // at least 16 B position + 1 B length VarInt + 1 B string minimum (18 B
    // for `MCF2`'s item-count VarInt), so the decodable count can never exceed
    // `rest.len() / 18`. The capacity is only a hint — a smaller `rest` still
    // decodes correctly, it just reallocates.
    let mut entries = Vec::with_capacity((count as usize).min(rest.len() / 18));
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
        let st = live_create_unresolved_state(canon)
            .or_else(|| registry.decode_canonical(canon))
            .or_else(|| live_mod_state_for_arg(canon))
            .ok_or(ERR)?;
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
                let name = std::str::from_utf8(&rest[..slen as usize]).map_err(|_| ERR)?;
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

// ===========================================================================
// AUTO-OPT: generic mod auto-optimizer + lazy catch-up + colossal/forge
// ===========================================================================

/// Memoizer machine kinds — keep the press and mixer recipe tables in
/// disjoint `(kind, fingerprint)` key spaces so an identical item layout
/// can't cross-hit between the two processors.
#[cfg(feature = "mod-suite")]
const LIVE_OPT_MACHINE_PRESS: u32 = 0;
#[cfg(feature = "mod-suite")]
const LIVE_OPT_MACHINE_MIXER: u32 = 1;

/// Process-wide auto-optimizer state backing live mod machine work:
/// recipe lookups memoized per `(kind, input-fingerprint)`, per-machine
/// poll cadences on the `1 -> 2 -> 4 -> 8 -> 20` backoff ladder, plus the
/// delta coalescer and bulk-section batcher the `live_opt_wire` helpers
/// expose to high-frequency mod sync traffic and bulk world edits.
#[cfg(feature = "mod-suite")]
struct LiveModAutoOptimizer {
    /// `(machine_kind, fingerprint) -> matched recipe index`. Misses are
    /// cached on equal footing, so an idle depot holding unmatched items
    /// rescans the recipe table exactly once until its contents change.
    recipes: AutoRecipeMemoizer<u32>,
    /// Per-machine poll cadence keyed by [`pack_machine_key`].
    backoff: AdaptivePollBackoff,
    /// Byte-identical sync-payload suppression + distance-tier throttle.
    coalescer: AutoDeltaCoalescer,
    /// Burst `set_block` coalescing into one update per 16³ section.
    batcher: AutoBulkSectionBatcher,
    /// `registry_revision` `item_tags` was exported at (`None` = never).
    tag_rev: Option<u64>,
    /// Flat item-tag member sets snapshot. The recipe matcher reads this
    /// inside [`AutoRecipeMemoizer::lookup_or_compute`] — keeping the
    /// compute closure free of suite-lock acquisitions is what lets the
    /// optimizer lock be held across it without ever forming a
    /// `suite -> optimizer` / `optimizer -> suite` cycle.
    item_tags: Arc<TagIndex>,
}

#[cfg(feature = "mod-suite")]
impl LiveModAutoOptimizer {
    fn new() -> Self {
        Self {
            recipes: AutoRecipeMemoizer::new(),
            backoff: AdaptivePollBackoff::new(),
            coalescer: AutoDeltaCoalescer::new(),
            batcher: AutoBulkSectionBatcher::new(),
            tag_rev: None,
            item_tags: Arc::new(BTreeMap::new()),
        }
    }
}

#[cfg(feature = "mod-suite")]
fn live_auto_optimizer() -> &'static Mutex<LiveModAutoOptimizer> {
    static OPT: OnceLock<Mutex<LiveModAutoOptimizer>> = OnceLock::new();
    OPT.get_or_init(|| Mutex::new(LiveModAutoOptimizer::new()))
}

/// Backoff table key for the machine at `pos` — `pack_machine_key` is
/// many-to-one by design: a collision only shares a cadence between two
/// positions, degrading the optimization rather than breaking it.
#[cfg(feature = "mod-suite")]
fn live_auto_machine_key(pos: FactoryPos) -> u64 {
    pack_machine_key(pos.dim, pos.x, pos.y, pos.z)
}

/// Whether the machine at `pos` should do its per-tick work on `tick`:
/// idle machines slide down the `1 -> 2 -> 4 -> 8 -> 20` ladder so a
/// depot holding unmatched items costs one backoff probe most ticks.
#[cfg(feature = "mod-suite")]
fn live_auto_machine_should_tick(pos: FactoryPos, tick: u64) -> bool {
    live_auto_optimizer()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .backoff
        .should_tick(live_auto_machine_key(pos), tick)
}

/// Report whether a granted poll changed observable machine state —
/// `true` snaps the cadence back to every tick, four consecutive `false`
/// advance one ladder step.
#[cfg(feature = "mod-suite")]
fn live_auto_machine_record(pos: FactoryPos, changed: bool) {
    live_auto_optimizer()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .backoff
        .record_tick_result(live_auto_machine_key(pos), changed);
}

/// Drop the cadence entry for a machine whose tracked slot vanished.
#[cfg(feature = "mod-suite")]
fn live_auto_machine_remove(pos: FactoryPos) {
    live_auto_optimizer()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .backoff
        .remove(live_auto_machine_key(pos));
}

/// Dirty-event wake for a batch of positions: inventory or structure
/// writes snap every listed machine's cadence back to 1 so a machine fed
/// while asleep never waits out its current ladder step.
#[cfg(feature = "mod-suite")]
fn live_auto_machine_wake_positions(positions: &[FactoryPos]) {
    if positions.is_empty() {
        return;
    }
    let keys: Vec<u64> = positions
        .iter()
        .map(|p| live_auto_machine_key(*p))
        .collect();
    live_auto_optimizer()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .backoff
        .wake_neighbors(&keys);
}

/// Stable fingerprint of a held-slot item multiset: `BTreeMap` ordering
/// makes equal contents hash equal regardless of seat order.
#[cfg(feature = "mod-suite")]
fn live_fingerprint_held_items(items: &[i64]) -> u64 {
    let mut counts: BTreeMap<u32, u32> = BTreeMap::new();
    for &item in items {
        if let Ok(id) = u32::try_from(item) {
            *counts.entry(id).or_insert(0) += 1;
        }
    }
    let slots: Vec<(u32, u32)> = counts.into_iter().collect();
    fingerprint_item_slots(&slots)
}

/// `live_create_ingredient_matches` resolved against a pinned flat tag
/// snapshot instead of the suite lock — the lock-free variant the
/// memoized recipe matcher runs inside `lookup_or_compute`.
#[cfg(feature = "mod-suite")]
fn live_create_ingredient_matches_tags(
    ingredient: &LiveCreateIngredient,
    item: i64,
    tags: &TagIndex,
) -> bool {
    match ingredient {
        LiveCreateIngredient::Item(id) => u32::try_from(item).ok() == Some(*id),
        LiveCreateIngredient::Tag(tag) => item_to_name(item).is_some_and(|name| {
            tags.get(tag.as_str())
                .or_else(|| tags.get(&canonical_common_tag_id(tag)))
                .is_some_and(|members| members.contains(name))
        }),
    }
}

/// Recipe match for a live mod machine, memoized per
/// `(machine_kind, held-items fingerprint)`. `compute` resolves the
/// recipe index against the pinned tag snapshot — it is the only code
/// that runs under the optimizer lock, and the snapshot refresh above
/// happens across sequential (never nested) suite/optimizer holds, so no
/// lock cycle exists even while a JAR rescan bumps `registry_revision`
/// mid-tick. A revision change invalidates the whole memo table here:
/// recipe/tag tables only ever move under `scan_mods_dir`, which bumps
/// the same counter.
#[cfg(feature = "mod-suite")]
fn live_auto_recipe_lookup(
    machine_kind: u32,
    fingerprint: u64,
    compute: impl FnOnce(&TagIndex) -> Option<u32>,
) -> Option<u32> {
    let rev = live_mod_suite()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .registry_revision;
    let stale = live_auto_optimizer()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .tag_rev
        != Some(rev);
    if stale {
        let flat = live_mod_suite()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .unified_tags
            .export_flat(TagDomain::Item);
        let mut opt = live_auto_optimizer()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        opt.item_tags = Arc::new(flat);
        opt.tag_rev = Some(rev);
        opt.recipes.invalidate_all();
    }
    let tags = live_auto_optimizer()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .item_tags
        .clone();
    live_auto_optimizer()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .recipes
        .lookup_or_compute(machine_kind, fingerprint, || compute(&tags))
}

/// Live `lazy_catchup` wiring: `CatchUpLimits` gated by
/// `MEGU_LAZY_CATCHUP` (fail-closed `enabled: false` out of the box for
/// strict Vanilla parity — every `catch_up_*` still clamps the delta
/// through `effective_delta`, so a disabled state is a true no-op), plus
/// the bounded `HeadlessFactoryRegistry` for unloaded-chunk factory
/// accumulation.
#[cfg(feature = "mod-suite")]
struct LiveLazyCatchUpState {
    limits: CatchUpLimits,
    factories: HeadlessFactoryRegistry,
    /// Chunk -> tick it left the warm set; read back on re-warm as the
    /// catch-up delta so unloaded time is settled exactly once.
    cold_since: HashMap<(i32, i32), u64>,
    /// Registered headless node_id -> its chunk (the registry's node map
    /// is private, so removal-by-chunk needs this side index).
    factory_chunks: HashMap<u64, (i32, i32)>,
}

#[cfg(feature = "mod-suite")]
impl Default for LiveLazyCatchUpState {
    fn default() -> Self {
        Self {
            limits: CatchUpLimits::default(),
            factories: HeadlessFactoryRegistry::new(),
            cold_since: HashMap::new(),
            factory_chunks: HashMap::new(),
        }
    }
}

#[cfg(feature = "mod-suite")]
fn live_lazy_catchup_state() -> &'static Mutex<LiveLazyCatchUpState> {
    static STATE: OnceLock<Mutex<LiveLazyCatchUpState>> = OnceLock::new();
    STATE.get_or_init(|| {
        let enabled = std::env::var("MEGU_LAZY_CATCHUP")
            .map(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false);
        Mutex::new(LiveLazyCatchUpState {
            limits: CatchUpLimits {
                enabled,
                ..CatchUpLimits::default()
            },
            factories: HeadlessFactoryRegistry::new(),
            cold_since: HashMap::new(),
            factory_chunks: HashMap::new(),
        })
    })
}

/// A forge registered in the live bridge: the `MultiBlockForgeRuntime`
/// behind the bus endpoint plus the spec it validated against, so
/// revalidation, breath application and tick stepping reuse the same
/// objects the hopper path feeds.
#[cfg(feature = "mod-suite")]
struct LiveForgeEntry {
    spec: MultiBlockForgeSpec,
    runtime: Arc<Mutex<MultiBlockForgeRuntime>>,
}

/// Live registry for `colossal_and_forge`: registered forge runtimes
/// (each also published as an `ItemTransferHandler` on `live_cap_bus`),
/// multi-part hitbox layouts for ray hit-tests, breath specs for cone
/// sweeps, and a `registry_revision`-keyed inverted block-tag export for
/// `BlockProbe` tag expectations.
#[cfg(feature = "mod-suite")]
#[derive(Default)]
struct LiveColossalAndForgeRegistry {
    forges: HashMap<BlockCoord, LiveForgeEntry>,
    hitboxes: HashMap<String, MultiPartHitbox>,
    breaths: HashMap<String, ElementalBreathSpec>,
    /// `registry_revision` `block_member_tags` was built at.
    tag_rev: Option<u64>,
    /// Inverted block-tag export: member block id -> every tag it
    /// carries (`export_flat` stores `forge:`/`neoforge:` aliases for
    /// `c:` tags, so all spellings land in the same member sets).
    block_member_tags: HashMap<String, BTreeSet<String>>,
}

#[cfg(feature = "mod-suite")]
fn live_colossal_registry() -> &'static Mutex<LiveColossalAndForgeRegistry> {
    static REG: OnceLock<Mutex<LiveColossalAndForgeRegistry>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(LiveColossalAndForgeRegistry::default()))
}

/// Snapshot matching coordinates before running work that re-enters the registry.
#[cfg(feature = "mod-suite")]
fn live_forge_cores(matches: impl Fn(&BlockCoord) -> bool) -> Vec<BlockCoord> {
    live_colossal_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .forges
        .keys()
        .filter(|coord| matches(coord))
        .cloned()
        .collect()
}

#[cfg(feature = "mod-suite")]
fn live_revalidate_forges(matches: impl Fn(&BlockCoord) -> bool) {
    for coord in live_forge_cores(matches) {
        if let Some(dim) = live_dim_for_coord(&coord) {
            let _ = live_forge_revalidate(dim, coord.x, coord.y, coord.z);
        }
    }
}

/// `BlockProbe` for the world cell at `pos` in `dim`: the live block id
/// (JAR registry-plan states first, then the `BReg` name table that
/// covers vanilla + Create states) plus every block-domain tag the id
/// carries, read off the flat-export snapshot pinned to
/// `registry_revision`. `None` when the state resolves to no id —
/// `ForgeCellExpect::Air` accepts exactly that answer, so unloaded or
/// unresolvable cells behave like air rather than failing the pattern.
/// All suite/registry holds are sequential (never nested).
#[cfg(feature = "mod-suite")]
fn live_block_probe(dim: DimId, pos: [i32; 3]) -> Option<BlockProbe> {
    let st = block_at_dim(dim, pos[0], pos[1], pos[2]);
    let id =
        live_mod_name_for_state(st).or_else(|| breg().map(|reg| reg.name_of(st).to_string()))?;
    if id.is_empty() {
        return None;
    }
    let rev = live_mod_suite()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .registry_revision;
    let stale = live_colossal_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .tag_rev
        != Some(rev);
    if stale {
        let flat = live_mod_suite()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .unified_tags
            .export_flat(TagDomain::Block);
        let mut reg = live_colossal_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        reg.block_member_tags.clear();
        for (tag, members) in flat {
            for member in members {
                reg.block_member_tags
                    .entry(member)
                    .or_default()
                    .insert(tag.clone());
            }
        }
        reg.tag_rev = Some(rev);
    }
    let tags = live_colossal_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .block_member_tags
        .get(&id)
        .cloned()
        .unwrap_or_default();
    Some(BlockProbe { id, tags })
}

// ---------------------------------------------------------------------------
// live_opt_wire surface: auto-optimizer, lazy catch-up, colossal + forge
// entry points. Consumers are live_create_*, live_mod_tick and the
// world-write hooks; the rest is the spec'd surface other subsystems call
// when their event arrives (an unloaded chunk returning, a forge block
// forming, a colossal entity being ray-tested).
// ---------------------------------------------------------------------------

/// Point-in-time diagnostic snapshot of the live auto-optimizer —
/// memoizer, backoff, coalescer and batcher counters in one read so
/// a single probe shows whether the optimizer is saving work or just
/// costing hash-map lookups.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg(feature = "mod-suite")]
pub(crate) struct LiveAutoOptimizerStats {
    pub memo_hits: u64,
    pub memo_negative_hits: u64,
    pub memo_misses: u64,
    pub memo_evictions: u64,
    pub memo_len: usize,
    pub backoff_tracked: u64,
    pub backoff_sleeping: u64,
    pub backoff_ticks_saved: u64,
    pub coalesce_sent: u64,
    pub coalesce_dropped_duplicate: u64,
    pub coalesce_throttled: u64,
    pub bulk_raw_writes: u64,
    pub bulk_coalesced_writes: u64,
    pub bulk_cancelled_writes: u64,
    pub bulk_sections_touched: u64,
}

/// Combined counters across all four auto-optimizer subsystems.
#[cfg(feature = "mod-suite")]
pub(crate) fn live_mod_auto_optimizer_stats() -> LiveAutoOptimizerStats {
    let opt = live_auto_optimizer()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let memo = opt.recipes.stats();
    let backoff = opt.backoff.stats();
    let coalesce = opt.coalescer.stats();
    let bulk = opt.batcher.stats();
    LiveAutoOptimizerStats {
        memo_hits: memo.hits,
        memo_negative_hits: memo.negative_hits,
        memo_misses: memo.misses,
        memo_evictions: memo.evictions,
        memo_len: opt.recipes.len(),
        backoff_tracked: backoff.total_tracked,
        backoff_sleeping: backoff.sleeping_count,
        backoff_ticks_saved: backoff.ticks_saved,
        coalesce_sent: coalesce.sent,
        coalesce_dropped_duplicate: coalesce.dropped_duplicate,
        coalesce_throttled: coalesce.throttled_distance,
        bulk_raw_writes: bulk.raw_writes,
        bulk_coalesced_writes: bulk.coalesced_writes,
        bulk_cancelled_writes: bulk.cancelled_writes,
        bulk_sections_touched: bulk.sections_touched,
    }
}

/// Reset the optimizer to empty — memoized recipes, cadences,
/// coalescer sources and pending section batches all drop. Tests use
/// it to keep process-global state from leaking across cases.
#[cfg(feature = "mod-suite")]
pub(crate) fn live_auto_optimizer_reset() {
    *live_auto_optimizer()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = LiveModAutoOptimizer::new();
}

/// Delta-coalescer gate for a live mod sync source: `tier` encodes
/// `0=Near, 1=Mid, 2=Far, other=OutOfRange`. `true` iff `payload`
/// should ship this tick — byte-identical repeats and within-window
/// churn are suppressed.
#[cfg(feature = "mod-suite")]
pub(crate) fn live_mod_delta_should_emit(
    source_id: u64,
    payload: &[u8],
    current_tick: u64,
    tier: u8,
) -> bool {
    let tier = match tier {
        0 => DistanceTier::Near,
        1 => DistanceTier::Mid,
        2 => DistanceTier::Far,
        _ => DistanceTier::OutOfRange,
    };
    matches!(
        live_auto_optimizer()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .coalescer
            .should_emit(source_id, payload, current_tick, tier),
        EmitDecision::Send
    )
}

/// Queue a light-relevant bulk world write into the section batcher.
#[cfg(feature = "mod-suite")]
pub(crate) fn live_mod_bulk_record_set_block(
    dim: DimId,
    x: i32,
    y: i32,
    z: i32,
    old_state: u32,
    new_state: u32,
) {
    live_auto_optimizer()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .batcher
        .record_set_block(dim.raw() as i32, x, y, z, old_state, new_state);
}

/// Queue a provably-light-stable bulk write (batch reports clean
/// lighting when nothing else set it).
#[cfg(feature = "mod-suite")]
pub(crate) fn live_mod_bulk_record_set_block_unlit(
    dim: DimId,
    x: i32,
    y: i32,
    z: i32,
    old_state: u32,
    new_state: u32,
) {
    live_auto_optimizer()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .batcher
        .record_set_block_unlit(dim.raw() as i32, x, y, z, old_state, new_state);
}

/// Drain every pending section batch — one net cell write per
/// position, last-write-wins, `A -> B -> A` pairs already cancelled.
#[cfg(feature = "mod-suite")]
pub(crate) fn live_mod_bulk_drain() -> Vec<BulkSectionBatch> {
    live_auto_optimizer()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .batcher
        .drain_section_batches()
}

/// Whether O(1) unloaded-chunk catch-up is armed
/// (`MEGU_LAZY_CATCHUP` or the explicit API below).
#[cfg(feature = "mod-suite")]
pub(crate) fn live_mod_lazy_catchup_enabled() -> bool {
    live_lazy_catchup_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .limits
        .enabled
}

/// Explicit arming switch over the fail-closed default — the
/// programmatic equivalent of `MEGU_LAZY_CATCHUP=1`.
#[cfg(feature = "mod-suite")]
pub(crate) fn live_mod_lazy_catchup_set_enabled(enabled: bool) {
    live_lazy_catchup_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .limits
        .enabled = enabled;
}

/// O(1) furnace catch-up for `delta_ticks` of unloaded time, bounded
/// by the live `CatchUpLimits` (disabled -> the state echoes back
/// with zero work reported).
#[cfg(feature = "mod-suite")]
pub(crate) fn live_mod_lazy_catchup_furnace(
    state: &FurnaceCatchUpState,
    delta_ticks: u64,
) -> FurnaceCatchUpReport {
    let limits = live_lazy_catchup_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .limits;
    catch_up_furnace(&limits, state, delta_ticks)
}

/// Deterministic growth-stage catch-up for an unloaded crop/breed —
/// same geometric-wait model the per-tick tick would produce.
#[cfg(feature = "mod-suite")]
pub(crate) fn live_mod_lazy_catchup_growth(
    current_stage: u8,
    max_stage: u8,
    random_tick_prob_per_tick: f64,
    delta_ticks: u64,
    seed: u64,
) -> u8 {
    let limits = live_lazy_catchup_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .limits;
    catch_up_growth_stage(
        &limits,
        current_stage,
        max_stage,
        random_tick_prob_per_tick,
        delta_ticks,
        seed,
    )
}

/// O(1) cycle-batched catch-up for a declarative machine snapshot.
#[cfg(feature = "mod-suite")]
pub(crate) fn live_mod_lazy_catchup_machine(
    state: &MachineCatchUpState,
    delta_ticks: u64,
) -> MachineCatchUpReport {
    let limits = live_lazy_catchup_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .limits;
    catch_up_machine_batch(&limits, state, delta_ticks)
}

/// Register a headless factory node (bounded by
/// `MAX_HEADLESS_NODES`); `true` on acceptance — duplicate ids and
/// registry overflow report `false`.
#[cfg(feature = "mod-suite")]
pub(crate) fn live_mod_headless_factory_register(node: HeadlessFactoryNode) -> bool {
    live_lazy_catchup_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .factories
        .add_node(node)
        .is_ok()
}

/// Remove a registered node; `true` when it existed.
#[cfg(feature = "mod-suite")]
pub(crate) fn live_mod_headless_factory_remove(node_id: u64) -> bool {
    live_lazy_catchup_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .factories
        .remove_node(node_id)
        .is_ok()
}

/// Registered node count.
#[cfg(feature = "mod-suite")]
pub(crate) fn live_mod_headless_factory_len() -> usize {
    live_lazy_catchup_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .factories
        .len()
}

/// Advance every registered node by `ticks` — sub-item rates carry
/// over in milliper-item units, and the same `effective_delta` gate
/// makes the whole step a no-op while catch-up is disabled. Returns
/// items moved input -> output.
#[cfg(feature = "mod-suite")]
pub(crate) fn live_mod_headless_factory_step(ticks: u64) -> u64 {
    let mut state = live_lazy_catchup_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let limits = state.limits;
    state
        .factories
        .step_headless_factories(&limits, ticks)
        .items_moved
}

/// Register (or replace) a forge core at `(x, y, z)` in `dim` under
/// `spec`: the runtime validates its shell against the live world
/// once, then publishes itself on the capability bus as an
/// `ItemTransferHandler` so vanilla hoppers, Create ducts and
/// cross-mod conduits push recipe inputs and pull smelted outputs
/// with zero extra glue. Returns the initial `structure_valid`
/// verdict — an invalid forge still registers (revalidation later
/// can arm it; breath and smelts stay refused meanwhile).
#[cfg(feature = "mod-suite")]
pub(crate) fn live_forge_register(
    dim: DimId,
    x: i32,
    y: i32,
    z: i32,
    spec: MultiBlockForgeSpec,
) -> bool {
    let coord = live_cap_coord(dim, x, y, z);
    let runtime = Arc::new(Mutex::new(MultiBlockForgeRuntime::new([x, y, z])));
    let valid = runtime
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .revalidate(&spec, &mut |pos| live_block_probe(dim, pos));
    live_cap_bus_guard().register_item_handler(
        coord.clone(),
        Box::new(LiveSharedItemEndpoint {
            inner: runtime.clone(),
        }),
    );
    live_colossal_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .forges
        .insert(coord, LiveForgeEntry { spec, runtime });
    valid
}

/// Drop the forge at `(x, y, z)`: the bus endpoint unregisters in the
/// same call so no hopper keeps feeding a shadow buffer.
#[cfg(feature = "mod-suite")]
pub(crate) fn live_forge_unregister(dim: DimId, x: i32, y: i32, z: i32) -> bool {
    let coord = live_cap_coord(dim, x, y, z);
    live_cap_bus_guard().unregister_item_handler(&coord);
    live_colossal_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .forges
        .remove(&coord)
        .is_some()
}

/// Fetch `(spec, runtime)` for a registered forge, or `None`.
#[cfg(feature = "mod-suite")]
fn live_forge_entry(
    dim: DimId,
    x: i32,
    y: i32,
    z: i32,
) -> Option<(MultiBlockForgeSpec, Arc<Mutex<MultiBlockForgeRuntime>>)> {
    let coord = live_cap_coord(dim, x, y, z);
    live_colossal_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .forges
        .get(&coord)
        .map(|e| (e.spec.clone(), e.runtime.clone()))
}

/// Re-run structure validation against the live world; `None` when
/// no forge is registered there.
#[cfg(feature = "mod-suite")]
pub(crate) fn live_forge_revalidate(dim: DimId, x: i32, y: i32, z: i32) -> Option<bool> {
    let (spec, runtime) = live_forge_entry(dim, x, y, z)?;
    let valid = runtime
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .revalidate(&spec, &mut |pos| live_block_probe(dim, pos));
    Some(valid)
}

/// One forge tick: advances the active smelt, applies idle decay,
/// and starts the next recipe when inputs and heat allow. `None`
/// when no forge is registered there.
#[cfg(feature = "mod-suite")]
pub(crate) fn live_forge_tick(dim: DimId, x: i32, y: i32, z: i32) -> Option<ForgeTickEvents> {
    let (spec, runtime) = live_forge_entry(dim, x, y, z)?;
    let events = runtime
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .tick(&spec);
    Some(events)
}

/// Apply `ticks` of `element` breath to the forge at `(x, y, z)`;
/// `None` when unregistered, `Some(false)` when the structure is
/// invalid or the element is not in `spec.heat_elements`.
#[cfg(feature = "mod-suite")]
pub(crate) fn live_forge_apply_breath(
    dim: DimId,
    x: i32,
    y: i32,
    z: i32,
    element: ElementKind,
    ticks: u32,
) -> Option<bool> {
    let (spec, runtime) = live_forge_entry(dim, x, y, z)?;
    let accepted = runtime
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .apply_breath(&spec, element, ticks);
    Some(accepted)
}

/// Register (or overwrite) a creature's multi-part layout under `id`.
#[cfg(feature = "mod-suite")]
pub(crate) fn live_colossal_register_hitbox(id: &str, hitbox: MultiPartHitbox) {
    live_colossal_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .hitboxes
        .insert(id.to_string(), hitbox);
}

/// Remove a registered layout; `true` when it existed.
#[cfg(feature = "mod-suite")]
pub(crate) fn live_colossal_unregister_hitbox(id: &str) -> bool {
    live_colossal_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .hitboxes
        .remove(id)
        .is_some()
}

/// Ray hit-test the registered hitbox `id` evaluated at `origin`
/// with `yaw_rad`/`pitch_rad`/`stage_scale` — nearest part struck
/// plus its resolved damage multiplier (`Head` 1.25 vs `Torso` 0.9
/// by default). `None` for unknown ids and clean misses alike.
#[allow(clippy::too_many_arguments)]
#[cfg(feature = "mod-suite")]
pub(crate) fn live_colossal_hit_test(
    id: &str,
    origin: [f64; 3],
    yaw_rad: f32,
    pitch_rad: f32,
    stage_scale: f32,
    ray_origin: [f64; 3],
    ray_dir: [f64; 3],
    max_dist: f64,
) -> Option<PartHit> {
    let hitbox = live_colossal_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .hitboxes
        .get(id)
        .cloned()?;
    let aabbs = hitbox.evaluate(origin, yaw_rad, pitch_rad, stage_scale);
    hitbox.hit_test(&aabbs, ray_origin, ray_dir, max_dist)
}

/// 1-based growth stage a `GrowthStageCurve` assigns `age_ticks` —
/// the same index a caller feeds back as `stage_scale` through
/// `spec_for_stage` when sizing a hit_test.
#[cfg(feature = "mod-suite")]
pub(crate) fn live_colossal_stage_at(curve: &GrowthStageCurve, age_ticks: u64) -> u8 {
    curve.stage_for_age_ticks(age_ticks)
}

/// Register (or overwrite) a breath profile under `id`.
#[cfg(feature = "mod-suite")]
pub(crate) fn live_breath_register(id: &str, spec: ElementalBreathSpec) {
    live_colossal_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .breaths
        .insert(id.to_string(), spec);
}

/// Evaluate the registered breath `id` from `mouth_origin` along
/// `(yaw_rad, pitch_rad)` against `entities` and the live world
/// (`live_block_probe` answers every terrain cell — bounded by the
/// spec's probe/transform budgets). `None` for unknown ids.
#[cfg(feature = "mod-suite")]
pub(crate) fn live_breath_evaluate(
    id: &str,
    dim: DimId,
    mouth_origin: [f64; 3],
    yaw_rad: f32,
    pitch_rad: f32,
    entities: &[BreathEntity],
) -> Option<BreathOutcome> {
    let spec = live_colossal_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .breaths
        .get(id)
        .cloned()?;
    Some(evaluate_breath_cone(
        mouth_origin,
        yaw_rad,
        pitch_rad,
        &spec,
        entities,
        &mut |pos| live_block_probe(dim, pos),
    ))
}

/// Resolve a `BlockCoord`'s string dimension id back to the engine
/// `DimId` that produced it (via `live_cap_coord`'s encoding), falling
/// back to the `megu:dim_<raw>` custom-dimension spelling.
#[cfg(feature = "mod-suite")]
fn live_dim_for_coord(coord: &BlockCoord) -> Option<DimId> {
    for dim in [DimId::Overworld, DimId::Nether, DimId::End] {
        if live_cap_coord(dim, coord.x, coord.y, coord.z).dim_id == coord.dim_id {
            return Some(dim);
        }
    }
    coord
        .dim_id
        .strip_prefix("megu:dim_")
        .and_then(|raw| raw.parse::<u32>().ok())
        .map(DimId::from_raw)
}

/// Sample the 26 neighbours of a just-placed forge core into a
/// `MultiBlockForgeSpec`: every occupied cell becomes an exact-match
/// expectation, empty cells expect air — later revalidations enforce the
/// same shape the builder placed. The breath element is derived from the
/// core's name (`ice`/`frost`, `lightning`/`storm`, else fire).
#[cfg(feature = "mod-suite")]
fn live_forge_spec_from_world(
    dim: DimId,
    x: i32,
    y: i32,
    z: i32,
    core_name: &str,
) -> MultiBlockForgeSpec {
    let mut cells = Vec::with_capacity(26);
    for dy in -1..=1 {
        for dz in -1..=1 {
            for dx in -1..=1 {
                if dx == 0 && dy == 0 && dz == 0 {
                    continue;
                }
                let expect = match live_block_probe(dim, [x + dx, y + dy, z + dz]) {
                    Some(probe)
                        if probe.id != "air"
                            && !probe.id.ends_with(":air")
                            && !probe.id.ends_with("_air") =>
                    {
                        ForgeCellExpect::Block(probe.id)
                    }
                    _ => ForgeCellExpect::Air,
                };
                cells.push(ForgePatternCell {
                    offset: [dx, dy, dz],
                    expect,
                });
            }
        }
    }
    let element = if core_name.contains("ice") || core_name.contains("frost") {
        ElementKind::Ice
    } else if core_name.contains("lightning") || core_name.contains("storm") {
        ElementKind::Lightning
    } else {
        ElementKind::Fire
    };
    MultiBlockForgeSpec {
        cells,
        heat_elements: vec![element],
        heat_capacity: 1000.0,
        heat_per_breath_tick: 4.0,
        idle_heat_decay_per_tick: 0.5,
        recipes: Vec::new(),
    }
}

/// Block-event wiring for the mod forge surface, called from
/// `live_block_changed` on every world write:
/// * a write into a registered forge's core cell drops the endpoint,
/// * placing a `*_forge_core` mod block registers a forge whose shell
///   pattern is sampled from the just-built structure,
/// * every write revalidates registered forges whose 3x3x3 shell it
///   touches — bounded by the endpoint count, not the world size.
#[cfg(feature = "mod-suite")]
fn live_mod_forge_on_block_set(dim: DimId, x: i32, y: i32, z: i32, old: u32, st: u32) {
    if old == st {
        return;
    }
    let coord = live_cap_coord(dim, x, y, z);
    if live_colossal_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .forges
        .contains_key(&coord)
    {
        let _ = live_forge_unregister(dim, x, y, z);
    }
    if let Some(name) = live_mod_name_for_state(st) {
        if name.ends_with("forge_core") {
            let _ = live_forge_register(
                dim,
                x,
                y,
                z,
                live_forge_spec_from_world(dim, x, y, z, &name),
            );
        }
    }
    live_revalidate_forges(|coord| {
        (coord.x - x).abs() <= 2 && (coord.y - y).abs() <= 2 && (coord.z - z).abs() <= 2
    });
    // Generic mod surface: JAR-declared blocks whose ids carry capability
    // vocabulary register bus endpoints, and `*cooking_pot` blocks arm or
    // retire live pots (heat re-evaluated from the blocks underneath).
    live_cap_sync_generic_endpoint(dim, x, y, z, st);
    let was_pot = live_mod_cooking_pot_name(old);
    let is_pot = live_mod_cooking_pot_name(st);
    if was_pot
        || is_pot
        || live_mod_cooking_pot_name(block_at_dim(dim, x, y + 1, z))
    {
        live_mod_sync_cooking_pots(dim, x, y, z, was_pot, is_pot);
    }
}

/// `true` when the state id names a `*cooking_pot` workstation — the
/// generic convention every culinary mod's pot block shares, matched on
/// the block path like `forge_core` above, never on a mod id.
#[cfg(feature = "mod-suite")]
fn live_mod_cooking_pot_name(st: u32) -> bool {
    live_mod_block_name_of(st).ends_with("cooking_pot")
}

/// Keep live cooking pots in step with one block write: a placed
/// `*cooking_pot` arms a fresh [`CookingPotState`] heated by whatever
/// sits beneath it, a broken/replaced pot retires its scheduler entry and
/// bus endpoint, a same-block state flip only re-evaluates heat, and a
/// write directly under a pot re-checks that pot's heat source. All pot
/// mutations funnel through [`LiveModSuite::set_cooking_pot`] so the
/// deadline scheduler and shared bus endpoint stay canonical.
#[cfg(feature = "mod-suite")]
fn live_mod_sync_cooking_pots(
    dim: DimId,
    x: i32,
    y: i32,
    z: i32,
    was_pot: bool,
    is_pot: bool,
) {
    let tick = TICK_IDX.load(Ordering::Relaxed);
    // A pot one cell above heats from the written block.
    let above_key = (dim, x, y + 1, z);
    let above_is_pot = live_mod_cooking_pot_name(block_at_dim(dim, x, y + 1, z));
    let mut suite = live_mod_suite().lock().unwrap_or_else(|e| e.into_inner());
    if !suite.active {
        return;
    }
    let heat_below = |suite: &LiveModSuite, px: i32, py: i32, pz: i32| {
        let below = live_mod_block_name_of(block_at_dim(dim, px, py - 1, pz));
        let below2 = live_mod_block_name_of(block_at_dim(dim, px, py - 2, pz));
        is_heated(&below, Some(&below2), &suite.culinary.tags)
    };
    let key = (dim, x, y, z);
    if is_pot {
        let heated = heat_below(&suite, x, y, z);
        let updated = suite
            .cooking_pots
            .get(&key)
            .and_then(|adapter| {
                let guard = adapter.lock().unwrap_or_else(|e| e.into_inner());
                let mut pot = guard.pot().clone();
                (pot.heated != heated).then(|| {
                    pot.heated = heated;
                    pot
                })
            });
        match updated {
            Some(pot) => suite.set_cooking_pot(key, pot, tick),
            None if !suite.cooking_pots.contains_key(&key) => {
                let mut pot = CookingPotState::new();
                pot.heated = heated;
                suite.set_cooking_pot(key, pot, tick);
            }
            None => {}
        }
    } else if was_pot {
        if let Some(adapter) = suite.cooking_pots.remove(&key) {
            if let Some(deadline) = adapter
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pot()
                .next_completion_tick
            {
                suite.cooking_deadlines.remove(&(deadline, dim, x, y, z));
            }
            live_cap_unregister(dim, x, y, z);
        }
    }
    if above_is_pot && suite.cooking_pots.contains_key(&above_key) {
        let heated = heat_below(&suite, x, y + 1, z);
        let updated = suite.cooking_pots.get(&above_key).and_then(|adapter| {
            let guard = adapter.lock().unwrap_or_else(|e| e.into_inner());
            let mut pot = guard.pot().clone();
            (pot.heated != heated).then(|| {
                pot.heated = heated;
                pot
            })
        });
        if let Some(pot) = updated {
            suite.set_cooking_pot(above_key, pot, tick);
        }
    }
}

/// Cold→warm transition handler wired from `Region::refresh_hot`. Chunks
/// leaving the warm set get a cold-since stamp plus headless bookkeeping
/// for their Create-held cells; chunks re-entering get bounded O(1)
/// catch-up applied to every deferred simulation (furnaces, crops,
/// headless endpoints) so unloaded time lands exactly once — the same
/// `MEGU_LAZY_CATCHUP` fail-closed gate as every lazy surface.
#[cfg(feature = "mod-suite")]
fn live_catchup_on_region_change(
    left_warm: &HashSet<(i32, i32)>,
    returned_warm: &HashSet<(i32, i32)>,
    now_tick: u64,
) {
    if !live_mod_lazy_catchup_enabled() || (left_warm.is_empty() && returned_warm.is_empty()) {
        return;
    }
    live_stamp_cold_chunks(left_warm, now_tick);
    for &(cx, cz) in returned_warm {
        let Some(start) = live_lazy_catchup_state()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .cold_since
            .remove(&(cx, cz))
        else {
            continue;
        };
        let delta = now_tick.saturating_sub(start);
        if delta == 0 {
            continue;
        }
        live_catchup_furnaces(cx, cz, delta);
        live_catchup_crops(cx, cz, delta);
        live_settle_headless_chunk(cx, cz, delta);
        // The forge shell may have changed while no live tick saw it.
        live_revalidate_forges(|coord| {
            coord.x.div_euclid(16) == cx && coord.z.div_euclid(16) == cz
        });
    }
}

/// Stamp chunks leaving the warm set and snapshot their Create-held cells.
#[cfg(feature = "mod-suite")]
fn live_stamp_cold_chunks(left_warm: &HashSet<(i32, i32)>, now_tick: u64) {
    if left_warm.is_empty() {
        return;
    }
    let mut cold = live_lazy_catchup_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for &chunk in left_warm {
        cold.cold_since.entry(chunk).or_insert(now_tick);
    }
    // Create depots/basins inside the leaving chunks get a headless
    // endpoint record so their buffered throughput survives the unload.
    if let Some(nodes) = live_create_with(|rt| {
        rt.held
            .iter()
            .filter(|(pos, _)| left_warm.contains(&(pos.x.div_euclid(16), pos.z.div_euclid(16))))
            .map(|(pos, held)| HeadlessFactoryNode {
                node_id: pack_machine_key(pos.dim, pos.x, pos.y, pos.z),
                dim_id: pos.dim as u32,
                cx: pos.x.div_euclid(16),
                cz: pos.z.div_euclid(16),
                rate_per_tick_milli: MILLI_PER_ITEM as u32,
                input_buffer: held.items.len() as u32,
                output_buffer: 0,
                max_buffer: 16,
                carry_milli: 0,
            })
            .collect::<Vec<_>>()
    }) {
        for node in nodes {
            let chunk = (node.cx, node.cz);
            if live_mod_headless_factory_register(node) {
                cold.factory_chunks.insert(node.node_id, chunk);
            }
        }
    }
}

/// Recover cook/burn time skipped by `furnace_tick` in cold chunks.
/// Like live smelting, the output must be empty or match the recipe.
#[cfg(feature = "mod-suite")]
fn live_catchup_furnaces(cx: i32, cz: i32, delta: u64) {
    let mut map = furnaces().lock().unwrap_or_else(|e| e.into_inner());
    for (&(_, x, _, z), furnace) in map.iter_mut() {
        if x.div_euclid(16) != cx || z.div_euclid(16) != cz {
            continue;
        }
        let Some((out_item, xp_each)) = smelt_out(furnace.input.0) else {
            continue;
        };
        if furnace.output.1 > 0 && furnace.output.0 != out_item {
            continue;
        }
        let report = live_mod_lazy_catchup_furnace(
            &FurnaceCatchUpState {
                input_count: u32::from(furnace.input.1),
                fuel_count: u32::from(furnace.fuel.1),
                fuel_burn_ticks_each: fuel_ticks(furnace.fuel.0).max(0) as u32,
                current_burn_remaining: furnace.burn_left.max(0) as u32,
                cook_progress: furnace.progress.max(0) as u32,
                cook_time_total: server_ticks(200) as u32,
                output_count: u32::from(furnace.output.1),
                output_max_stack: stack_max_of(&furnace.output).max(0) as u32,
            },
            delta,
        );
        if report.items_smelted > 0 {
            furnace.output = (
                out_item,
                report.state.output_count.min(u8::MAX as u32) as u8,
                furnace.output.2.clone(),
            );
            furnace.xp += xp_each as f32 * report.items_smelted as f32;
        }
        furnace.input.1 = report.state.input_count.min(u8::MAX as u32) as u8;
        furnace.fuel.1 = report.state.fuel_count.min(u8::MAX as u32) as u8;
        furnace.burn_left = report.state.current_burn_remaining as i32;
        furnace.progress = report.state.cook_progress as i32;
        if report.items_smelted > 0 || report.fuel_consumed > 0 {
            furnace.dirty = true;
        }
    }
}

/// Settle missed random-growth chances deterministically per position.
#[cfg(feature = "mod-suite")]
fn live_catchup_crops(cx: i32, cz: i32, delta: u64) {
    let mut map = crops().lock().unwrap_or_else(|e| e.into_inner());
    let mut grown_cells = Vec::new();
    for (pos, age) in map.iter_mut() {
        let (_, x, y, z) = *pos;
        if x.div_euclid(16) != cx || z.div_euclid(16) != cz {
            continue;
        }
        let seed = ((x as i64).wrapping_mul(31) ^ (z as i64).wrapping_mul(37) ^ (y as i64)) as u64;
        let grown = live_mod_lazy_catchup_growth(
            *age,
            7,
            1.0 / CROP_GROWTH_DENOMINATOR as f64,
            delta,
            seed,
        );
        if grown > *age {
            *age = grown;
            grown_cells.push((pos.0, x, y, z, 4278 + grown as u32));
        }
    }
    drop(map);
    for (dim, x, y, z, state) in grown_cells {
        set_block_dim(dim, x, y, z, state);
        note_block_update_dim(dim, x, y, z, state);
    }
}

/// Settle buffered throughput once, then return the chunk to live ticking.
#[cfg(feature = "mod-suite")]
fn live_settle_headless_chunk(cx: i32, cz: i32, delta: u64) {
    let mut settled_cycles = 0u64;
    {
        let mut cold = live_lazy_catchup_state()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let ids: Vec<u64> = cold
            .factory_chunks
            .iter()
            .filter(|(_, chunk)| **chunk == (cx, cz))
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            cold.factory_chunks.remove(&id);
            let node = cold.factories.get(id).copied();
            if live_mod_headless_factory_remove(id) {
                if let Some(node) = node {
                    let ticks_per_cycle =
                        (MILLI_PER_ITEM / u64::from(node.rate_per_tick_milli).max(1)).max(1);
                    let report = live_mod_lazy_catchup_machine(
                        &MachineCatchUpState {
                            input_count: node.input_buffer,
                            energy: 0,
                            energy_per_cycle: 0,
                            ticks_per_cycle,
                            cycle_progress: (node.carry_milli * ticks_per_cycle) / MILLI_PER_ITEM,
                            output_count: node.output_buffer,
                            output_space: node.max_buffer.saturating_sub(node.output_buffer),
                        },
                        delta,
                    );
                    settled_cycles += report.cycles_completed;
                }
            }
        }
    }
    if settled_cycles > 0 {
        eprintln!(
            "[live-mods] lazy catch-up: chunk ({cx},{cz}) settled \
                 {settled_cycles} headless cycles over {delta}t"
        );
    }
}

/// Melee ray against the registered multi-part layout of `m`: returns
/// the struck part's damage multiplier, `1.0` when the mob has no
/// registered layout or the ray misses every part. Wired into the `0x16`
/// attack handler's mob branch.
#[cfg(feature = "mod-suite")]
fn live_colossal_attack_scale(
    ray_origin: [f64; 3],
    ray_dir: [f64; 3],
    max_dist: f64,
    m: &Mob,
) -> f32 {
    if m.etype != 31 {
        return 1.0;
    }
    let curve = GrowthStageCurve::default();
    // Dragons in this world are always fully grown — the curve's final
    // stage anchor is the adult hitbox scale.
    let stage = live_colossal_stage_at(&curve, u64::MAX);
    let stage_scale = curve.spec_for_stage(stage).hitbox_scale;
    let (x, y, z, yaw, pitch) = *m.pos.lock().unwrap_or_else(|e| e.into_inner());
    live_colossal_hit_test(
        LIVE_ENDER_DRAGON_ID,
        [x, y, z],
        yaw.to_radians(),
        pitch.to_radians(),
        stage_scale,
        ray_origin,
        ray_dir,
        max_dist,
    )
    .map(|hit| hit.damage_multiplier)
    .unwrap_or(1.0)
}

#[cfg(feature = "mod-suite")]
fn live_breath_distance_sq([x, y, z]: [f64; 3], origin: [f64; 3]) -> f64 {
    (x - origin[0]).powi(2) + (y - origin[1]).powi(2) + (z - origin[2]).powi(2)
}

/// Dragon fireball impact -> registered-forge heating, wired from
/// `live_dragon_breath`: the registered fire-breath cone is evaluated
/// over the world at the impact point (nearby mobs are offered as cone
/// candidates) and every forge inside the swept range gains fire-heat —
/// the Ice & Fire style "dragon breath powers forges" loop.
#[cfg(feature = "mod-suite")]
fn live_dragon_breath_mods(dim: DimId, pos: [f64; 3]) {
    let entities: Vec<BreathEntity> = mobs()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|m| m.dim == dim)
        .filter_map(|m| {
            let (x, y, z, _, _) = *m.pos.lock().unwrap_or_else(|e| e.into_inner());
            (live_breath_distance_sq([x, y, z], pos) <= 20.0 * 20.0).then_some(BreathEntity {
                entity_id: m.eid as u64,
                center: [x, y, z],
                half_extents: [0.5, 0.9, 0.5],
                conductive: false,
            })
        })
        .take(32)
        .collect();
    // The fireball plunges — the cone is aimed straight down at the
    // impact cell.
    let Some(outcome) = live_breath_evaluate(
        LIVE_ENDER_DRAGON_ID,
        dim,
        pos,
        0.0,
        -std::f32::consts::FRAC_PI_2,
        &entities,
    ) else {
        return;
    };
    let mut range = 16.0f64;
    for transform in &outcome.block_transforms {
        range = range.max(live_breath_distance_sq(transform.pos.map(f64::from), pos).sqrt());
    }
    for coord in live_forge_cores(|coord| {
        let center = [coord.x, coord.y, coord.z].map(|axis| f64::from(axis) + 0.5);
        live_breath_distance_sq(center, pos) <= range * range
    }) {
        if let Some(near_dim) = live_dim_for_coord(&coord) {
            if live_forge_entry(near_dim, coord.x, coord.y, coord.z).is_none() {
                continue;
            }
            let _ =
                live_forge_apply_breath(near_dim, coord.x, coord.y, coord.z, ElementKind::Fire, 40);
        }
    }
}

// Default-build stubs for the live_opt_wire surface: same names, no-op
// bodies, generic parameters where the real signature names a mod-suite
// type — matching the file's flat-stub convention.

/// Zeroed snapshot counterpart of the mod-suite stats struct.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg(not(feature = "mod-suite"))]
pub(crate) struct LiveAutoOptimizerStats;

#[inline(always)]
#[cfg(not(feature = "mod-suite"))]
pub(crate) fn live_mod_auto_optimizer_stats() -> LiveAutoOptimizerStats {
    LiveAutoOptimizerStats
}

#[inline(always)]
#[cfg(not(feature = "mod-suite"))]
pub(crate) fn live_mod_bulk_record_set_block(
    _dim: DimId,
    _x: i32,
    _y: i32,
    _z: i32,
    _old_state: u32,
    _new_state: u32,
) {
}

#[inline(always)]
#[cfg(not(feature = "mod-suite"))]
pub(crate) fn live_mod_bulk_record_set_block_unlit(
    _dim: DimId,
    _x: i32,
    _y: i32,
    _z: i32,
    _old_state: u32,
    _new_state: u32,
) {
}

#[inline(always)]
#[cfg(not(feature = "mod-suite"))]
pub(crate) fn live_mod_bulk_drain<T>() -> Vec<T> {
    Vec::new()
}

#[inline(always)]
#[cfg(not(feature = "mod-suite"))]
pub(crate) fn live_mod_lazy_catchup_enabled() -> bool {
    false
}

#[inline(always)]
#[cfg(not(feature = "mod-suite"))]
pub(crate) fn live_mod_lazy_catchup_set_enabled(_enabled: bool) {}

#[inline(always)]
#[cfg(not(feature = "mod-suite"))]
pub(crate) fn live_mod_lazy_catchup_furnace<S, R>(_state: &S, _delta_ticks: u64) -> Option<R> {
    None
}

#[inline(always)]
#[cfg(not(feature = "mod-suite"))]
pub(crate) fn live_mod_lazy_catchup_growth(
    current_stage: u8,
    _max_stage: u8,
    _random_tick_prob_per_tick: f64,
    _delta_ticks: u64,
    _seed: u64,
) -> u8 {
    current_stage
}

#[inline(always)]
#[cfg(not(feature = "mod-suite"))]
pub(crate) fn live_mod_lazy_catchup_machine<S, R>(_state: &S, _delta_ticks: u64) -> Option<R> {
    None
}

#[inline(always)]
#[cfg(not(feature = "mod-suite"))]
pub(crate) fn live_mod_headless_factory_register<N>(_node: N) -> bool {
    false
}

#[inline(always)]
#[cfg(not(feature = "mod-suite"))]
pub(crate) fn live_mod_headless_factory_remove(_node_id: u64) -> bool {
    false
}

#[inline(always)]
#[cfg(not(feature = "mod-suite"))]
pub(crate) fn live_mod_headless_factory_len() -> usize {
    0
}

#[inline(always)]
#[cfg(not(feature = "mod-suite"))]
pub(crate) fn live_mod_headless_factory_step(_ticks: u64) -> u64 {
    0
}

#[inline(always)]
#[cfg(not(feature = "mod-suite"))]
pub(crate) fn live_forge_register<S>(_dim: DimId, _x: i32, _y: i32, _z: i32, _spec: S) -> bool {
    false
}

#[inline(always)]
#[cfg(not(feature = "mod-suite"))]
pub(crate) fn live_forge_unregister(_dim: DimId, _x: i32, _y: i32, _z: i32) -> bool {
    false
}

#[inline(always)]
#[cfg(not(feature = "mod-suite"))]
pub(crate) fn live_forge_revalidate(_dim: DimId, _x: i32, _y: i32, _z: i32) -> Option<bool> {
    None
}

#[inline(always)]
#[cfg(not(feature = "mod-suite"))]
pub(crate) fn live_forge_tick<T>(_dim: DimId, _x: i32, _y: i32, _z: i32) -> Option<T> {
    None
}

#[inline(always)]
#[cfg(not(feature = "mod-suite"))]
pub(crate) fn live_forge_apply_breath<E>(
    _dim: DimId,
    _x: i32,
    _y: i32,
    _z: i32,
    _element: E,
    _ticks: u32,
) -> Option<bool> {
    None
}

#[inline(always)]
#[cfg(not(feature = "mod-suite"))]
pub(crate) fn live_colossal_register_hitbox<H>(_id: &str, _hitbox: H) {}

#[inline(always)]
#[cfg(not(feature = "mod-suite"))]
pub(crate) fn live_colossal_unregister_hitbox(_id: &str) -> bool {
    false
}

#[inline(always)]
#[allow(clippy::too_many_arguments)]
#[cfg(not(feature = "mod-suite"))]
pub(crate) fn live_colossal_hit_test<H>(
    _id: &str,
    _origin: [f64; 3],
    _yaw_rad: f32,
    _pitch_rad: f32,
    _stage_scale: f32,
    _ray_origin: [f64; 3],
    _ray_dir: [f64; 3],
    _max_dist: f64,
) -> Option<H> {
    None
}

#[inline(always)]
#[cfg(not(feature = "mod-suite"))]
pub(crate) fn live_colossal_stage_at<C>(_curve: &C, _age_ticks: u64) -> u8 {
    1
}

#[inline(always)]
#[cfg(not(feature = "mod-suite"))]
pub(crate) fn live_breath_register<S>(_id: &str, _spec: S) {}

#[inline(always)]
#[cfg(not(feature = "mod-suite"))]
pub(crate) fn live_breath_evaluate<O, E>(
    _id: &str,
    _dim: DimId,
    _mouth_origin: [f64; 3],
    _yaw_rad: f32,
    _pitch_rad: f32,
    _entities: &[E],
) -> Option<O> {
    None
}
/// Stub counterpart of `live_mod_forge_on_block_set`: the generic no-op
/// surface is exercised identically so a default build cannot silently
/// diverge — only the register attempt is gated on a state id no real
/// block reaches, keeping the inert registry empty.
#[cfg(not(feature = "mod-suite"))]
#[inline(always)]
fn live_mod_forge_on_block_set(dim: DimId, x: i32, y: i32, z: i32, _old: u32, st: u32) {
    let _ = live_forge_unregister(dim, x, y, z);
    let _ = live_forge_revalidate(dim, x, y, z);
    if st == u32::MAX {
        let _ = live_forge_register::<u8>(dim, x, y, z, 0u8);
    }
}

/// Stub counterpart of `live_catchup_on_region_change`: same gate, same
/// no-op surface — `MEGU_LAZY_CATCHUP` still governs and the generic
/// stubs return their empty values.
#[cfg(not(feature = "mod-suite"))]
fn live_catchup_on_region_change(
    _left_warm: &HashSet<(i32, i32)>,
    _returned_warm: &HashSet<(i32, i32)>,
    _now_tick: u64,
) {
    if !live_mod_lazy_catchup_enabled() {
        return;
    }
    let _ = live_mod_lazy_catchup_furnace::<u8, u8>(&0u8, _now_tick);
    let _ = live_mod_lazy_catchup_growth(0, 0, 0.0, _now_tick, 0);
    let _ = live_mod_lazy_catchup_machine::<u8, u8>(&0u8, _now_tick);
    let _ = live_mod_headless_factory_register::<u8>(0u8);
    let _ = live_mod_headless_factory_remove(0);
}

/// Stub counterpart of `live_colossal_attack_scale`: `1.0` always — no
/// registered layouts exist without `mod-suite`.
#[cfg(not(feature = "mod-suite"))]
#[inline(always)]
fn live_colossal_attack_scale(
    ray_origin: [f64; 3],
    ray_dir: [f64; 3],
    max_dist: f64,
    _m: &Mob,
) -> f32 {
    let _ = live_colossal_stage_at(&(), 0);
    let _ =
        live_colossal_hit_test::<u8>("", ray_origin, 0.0, 0.0, 1.0, ray_origin, ray_dir, max_dist);
    1.0
}

/// Stub counterpart of `live_dragon_breath_mods`: no registered breath
/// profiles or forges exist without `mod-suite`.
#[cfg(not(feature = "mod-suite"))]
#[inline(always)]
fn live_dragon_breath_mods(dim: DimId, pos: [f64; 3]) {
    let _: Option<u8> = live_breath_evaluate::<u8, u8>("", dim, pos, 0.0, 0.0, &[]);
    let _ = live_forge_apply_breath::<u8>(dim, 0, 0, 0, 0u8, 0);
}

#[cfg(not(feature = "mod-suite"))]
#[inline(always)]
fn live_create_on_block_set(_dim: DimId, _x: i32, _y: i32, _z: i32, _old: u32, _st: u32) {}

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

/// Default builds do not emit mod custom payloads.
#[cfg(all(test, not(feature = "mod-suite")))]
#[inline(always)]
fn live_mod_custom_payload_frame(_channel: &str, _payload: &[u8]) -> Vec<u8> {
    Vec::new()
}

#[cfg(all(test, not(feature = "mod-suite")))]
#[inline(always)]
fn live_mod_magic_absorb_damage(_uuid: [u8; 16], damage: f32) -> f32 {
    damage
}

#[cfg(all(test, not(feature = "mod-suite")))]
#[inline(always)]
fn live_mod_magic_tick_player(_uuid: [u8; 16]) -> Option<()> {
    None
}

#[cfg(all(test, not(feature = "mod-suite")))]
#[inline(always)]
fn live_mod_magic_remove_player(_uuid: [u8; 16]) -> bool {
    false
}

/// Stub counterpart of `live_auto_optimizer_reset`: no optimizer state
/// exists without `mod-suite`.
#[cfg(not(feature = "mod-suite"))]
#[inline(always)]
pub(crate) fn live_auto_optimizer_reset() {}

/// No-op without `mod-suite`: no mods dir scan, no classification — the
/// shared arming surface still runs so both builds reset/arm the same way.
#[cfg(not(feature = "mod-suite"))]
#[inline(always)]
fn live_mod_suite_boot() {
    live_auto_optimizer_reset();
    live_mod_lazy_catchup_set_enabled(false);
    live_colossal_unregister_hitbox(LIVE_ENDER_DRAGON_ID);
    live_colossal_register_hitbox(LIVE_ENDER_DRAGON_ID, ());
    live_breath_register(LIVE_ENDER_DRAGON_ID, ());
}

/// No-op without `mod-suite`: no mod-sync manifest is ever armed.
#[cfg(not(feature = "mod-suite"))]
#[inline(always)]
fn live_mod_sync_on_join(_s: &mut TcpStream) {}

/// No-op without `mod-suite`: no dynamic registry frame is ever emitted.
#[cfg(not(feature = "mod-suite"))]
#[inline(always)]
fn live_mod_registry_sync_on_join(_s: &mut TcpStream) {}

/// No-op without `mod-suite`: the tick hook contributes nothing — the
/// shared optimizer/forge tick surface is still drained so both builds
/// walk the same code path.
#[cfg(not(feature = "mod-suite"))]
#[inline(always)]
fn live_mod_tick() {
    let _ = live_mod_headless_factory_step(1);
    let _: Vec<u8> = live_mod_bulk_drain::<u8>();
    let _ = live_forge_tick::<u8>(DimId::Overworld, 0, 0, 0);
    let _ = live_mod_headless_factory_len();
    let _ = live_mod_auto_optimizer_stats();
}

/// Pass-through without `mod-suite`: no mod damage hooks exist, so the hit
/// applies exactly as vanilla computed it.
#[cfg(not(feature = "mod-suite"))]
#[inline(always)]
fn live_mod_entity_damage_pre(amount: f32) -> Option<f32> {
    Some(amount)
}

/// `Allowed` without `mod-suite`: no barrier engine exists to consult.
#[cfg(all(test, not(feature = "mod-suite")))]
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
fn live_take_orbital_transitions() -> Vec<create_adapter::orbital::ShipDimensionTransition> {
    mod_offload_bridge()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .offload
        .take_orbital_transitions()
}

#[cfg(feature = "mod-suite")]
pub fn live_format_romaji_chat(raw: &str, mode: megu_core::romaji_ime::RomajiChatMode) -> String {
    let cfg = megu_core::romaji_ime::RomajiChatConfig {
        mode,
        ..megu_core::romaji_ime::RomajiChatConfig::default()
    };
    megu_core::romaji_ime::format_romaji_chat(raw, &cfg)
}

#[cfg(feature = "mod-suite")]
fn live_tier25_batch_coalescer() -> &'static Mutex<megu_core::tier25_guard_batch::TickBatchCoalescer>
{
    static COALESCER: OnceLock<Mutex<megu_core::tier25_guard_batch::TickBatchCoalescer>> =
        OnceLock::new();
    COALESCER.get_or_init(|| Mutex::new(megu_core::tier25_guard_batch::TickBatchCoalescer::new()))
}

#[cfg(feature = "mod-suite")]
pub fn live_tier25_submit_guarded(
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
        LIVE_SERVER_BUDGET_SCALE.store((rt.budget_scale as f32).to_bits(), Ordering::Relaxed);
        rt.ticks_observed = rt.ticks_observed.saturating_add(1);
    }
}

#[cfg(not(feature = "mod-suite"))]
#[inline]
fn live_observe_server_tick_mspt(_mspt: f64) {}

/// Lock-free mirror of `ServerMathOptRuntime::budget_scale` — the spinning PID
/// output — for per-tick consumers that must not take the runtime mutex
/// (e.g. the interest-stream burst cap in `play_protocol.rs`). Written as
/// `f32::to_bits` on every PID update; `1.0` until the first observation, and
/// forever when `mod-suite` is disabled, so default builds shed nothing.
static LIVE_SERVER_BUDGET_SCALE: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(1.0f32.to_bits());

/// The server's current runtime load-shed factor, clamped to `[0.25, 1.0]`:
/// `1.0` means no shedding, `0.25` is the floor the PID can reach. Non-finite
/// payloads read as `1.0`.
#[inline]
pub(crate) fn live_server_budget_scale() -> f32 {
    let scale = f32::from_bits(LIVE_SERVER_BUDGET_SCALE.load(Ordering::Relaxed));
    if scale.is_finite() {
        scale.clamp(0.25, 1.0)
    } else {
        1.0
    }
}

#[cfg(feature = "mod-suite")]
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

    pub(super) static MAGIC_TEST_LOCK: Mutex<()> = Mutex::new(());

    pub(super) fn reset_magic() {
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
        {
            let suite = live_mod_suite();
            let mut guard = suite.lock().unwrap_or_else(|e| e.into_inner());
            *guard = LiveModSuite::new();
            // Recover the replaced test fixture, not production lock failures.
            suite.clear_poison();
        }
        live_cap_reset();
        live_mod_refresh_hook_gate();
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
        megu_core::hash::crc32_ieee(data)
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

    fn lmb_loot_glm_jar(dep_manifest: &str) -> Vec<u8> {
        let loot = br#"{"type":"minecraft:chest","pools":[{"rolls":1,"entries":[{"type":"minecraft:item","name":"minecraft:diamond"}]}]}"#;
        let global = br#"{"replace":false,"entries":["custom_mod:add_emerald"]}"#;
        let glm = br#"{"type":"neoforge:add_item","item":"minecraft:emerald","count":1}"#;
        let manifest = format!(
            r#"{{"schemaVersion":1,"id":"custom_mod","version":"1.0.0","environment":"*"{dep_manifest}}}"#
        );
        lmb_test_jar(&[
            ("fabric.mod.json", manifest.as_bytes()),
            ("data/minecraft/loot_table/chests/simple_dungeon.json", loot),
            (
                "data/neoforge/loot_modifiers/global_loot_modifiers.json",
                global,
            ),
            ("data/custom_mod/loot_modifiers/add_emerald.json", glm),
            (
                "data/custom_mod/tags/item/m3c_blocked.json",
                br#"{"values":["minecraft:emerald"]}"#,
            ),
        ])
    }

    #[test]
    fn m3c_mod_loot_overlay_glm_and_provenance_are_admitted() {
        let _serial = WIRE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        fresh_suite();
        let dir = lmb_temp_dir("m3c-loot-glm");
        std::fs::write(dir.join("custom-mod-1.jar"), lmb_loot_glm_jar("")).unwrap();
        let mut suite = LiveModSuite::new();
        suite.scan_mods_dir(&dir);
        assert!(suite.admitted_jars.contains("custom-mod-1.jar"));
        assert!(suite
            .loot_tables_raw
            .contains_key("minecraft:chests/simple_dungeon"));
        assert!(suite.loot_provenance.records().iter().any(|record| {
            record.domain == DiffDomain::LootTable
                && record.resource_id == "minecraft:chests/simple_dungeon"
                && record.action == ProvenanceAction::Overridden
                && record.source_layer == "custom_mod"
                && record.previous_owner.as_deref() == Some("vanilla")
        }));
        assert!(suite
            .glm_engine
            .active_order()
            .iter()
            .any(|id| id == "custom_mod:add_emerald"));
        suite.active = true;
        *live_mod_suite().lock().unwrap_or_else(|e| e.into_inner()) = suite;
        let drops = apply_live_mod_loot_modifiers(
            "minecraft:chests/simple_dungeon",
            DimId::Overworld,
            Some((1, 64, 1)),
            false,
            None,
            Vec::new(),
            7,
        );
        assert!(drops.iter().any(|(item, count, _)| {
            *count == 1 && matches!(item_to_name(*item), Some("emerald" | "minecraft:emerald"))
        }));
        let diff = live_provenance_diff_json();
        assert!(diff.contains("minecraft:chests/simple_dungeon"));
        assert!(diff.contains("custom_mod:add_emerald"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn m3c_missing_dependency_blocks_loot_tags_recipes_and_hooks() {
        let _serial = WIRE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        fresh_suite();
        let dir = lmb_temp_dir("m3c-missing-dep");
        std::fs::write(
            dir.join("custom-mod-1.jar"),
            lmb_loot_glm_jar(r#","depends":{"missing_lib":">=1.0"}"#),
        )
        .unwrap();
        let mut suite = LiveModSuite::new();
        suite.scan_mods_dir(&dir);
        assert!(!suite.admitted_jars.contains("custom-mod-1.jar"));
        assert!(!suite
            .loot_tables_raw
            .contains_key("minecraft:chests/simple_dungeon"));
        assert!(suite.glm_engine.active_order().is_empty());
        assert!(suite.damage_hooks.is_empty());
        assert!(!suite
            .unified_tags
            .export_flat(TagDomain::Item)
            .contains_key("custom_mod:m3c_blocked"));
        assert!(!suite.dependency_resolution.missing_required.is_empty());
        assert!(suite.reports.iter().any(|report| {
            report.file_name == "custom-mod-1.jar"
                && report.diagnostic.contains("dependency admission")
        }));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn m3c_kinetic_frames_chunk_and_stay_dimension_scoped() {
        let mut tags = Vec::new();
        for index in 0..4_097i32 {
            tags.push(create_adapter::factory::KineticUpdateTag {
                pos: FactoryPos::new(0, index, 64, 0),
                speed_rpm: 16.0,
                stress_capacity: 1.0,
                stress_load: 0.5,
                overstressed: false,
            });
        }
        tags.push(create_adapter::factory::KineticUpdateTag {
            pos: FactoryPos::new(1, 0, 64, 0),
            speed_rpm: 16.0,
            stress_capacity: 1.0,
            stress_load: 0.5,
            overstressed: false,
        });
        let frames = create_adapter::neoforge_wire::encode_kinetic_batches(&tags, 99);
        let chunks: Vec<_> = frames
            .iter()
            .map(|frame| {
                create_adapter::neoforge_wire::decode_kinetic_batch_chunk(frame)
                    .expect("valid kinetic chunk")
            })
            .collect();
        assert!(chunks.iter().all(|chunk| chunk.tags.len() <= 4096));
        assert_eq!(
            chunks
                .iter()
                .filter(|chunk| chunk.dimension_id == 0)
                .map(|chunk| chunk.tags.len())
                .sum::<usize>(),
            4097
        );
        assert_eq!(
            chunks
                .iter()
                .filter(|chunk| chunk.dimension_id == 1)
                .map(|chunk| chunk.tags.len())
                .sum::<usize>(),
            1
        );
        assert!(chunks
            .iter()
            .filter(|chunk| chunk.dimension_id == 0)
            .all(|chunk| chunk.chunk_total == 2));
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
            generic_kill_pending: AtomicBool::new(false),
            use_on_resend: Mutex::new(None),
        });
        (me, peer)
    }

    /// Read one framed server packet from the client peer. TCP may coalesce
    /// or split frames, so `stash` buffers bytes across calls: only the
    /// varint-delimited frame is consumed per call.
    pub(super) fn lmb_read_frame(
        peer: &mut TcpStream,
        stash: &mut Vec<u8>,
    ) -> Option<(i64, Vec<u8>)> {
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
            if n == 0 {
                return None;
            }
            stash.extend_from_slice(&buf[..n]);
        }
    }

    /// Decode a `custom_payload` S2C body into `(channel, payload)`.
    pub(super) fn lmb_decode_payload(body: &[u8]) -> (String, Vec<u8>) {
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
        let silent = peer.read(&mut [0u8; 8]).is_err();
        peer.set_read_timeout(Some(Duration::from_secs(5))).ok();
        silent
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
        std::fs::write(
            dir.join("sodium-0.6.jar"),
            lmb_test_jar(&[("fabric.mod.json", &lmb_fabric_manifest("sodium"))]),
        )
        .unwrap();
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
        let (adapted, cooking_recipes, hooks, reports) = {
            let suite = live_mod_suite().lock().unwrap_or_else(|e| e.into_inner());
            (
                suite.auto_adapted.clone(),
                suite.culinary_stats.cooking_recipes,
                suite.lifted_hooks.clone(),
                suite.reports.clone(),
            )
        };
        fresh_suite();
        std::fs::remove_dir_all(dir).unwrap();

        // The active list excludes both skipped and Tier 3 JARs; they still
        // have report rows. Only the verified Tier 2 JAR installs a hook.
        assert_eq!(adapted.len(), 2);
        assert_eq!(cooking_recipes, 1);
        assert_eq!(
            adapted
                .iter()
                .find(|m| m.mod_id == "delightaddon")
                .unwrap()
                .tag_files,
            1
        );
        let safe = adapted.iter().find(|m| m.mod_id == "safemod").unwrap();
        assert_eq!(safe.tier, AdaptationTier::Tier2LiftedShim);
        assert_eq!(safe.lifted_hooks.len(), 1);
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].1.hook, StandardEventHook::EntityDamagePre);
        assert!(!adapted.iter().any(|m| m.mod_id == "redirectmod"));
        let fallback = reports.iter().find(|r| r.mod_id == "redirectmod").unwrap();
        assert_eq!(fallback.tier, AdaptationTier::Tier3FallbackRequired);
        assert!(reports.iter().any(|r| {
            r.mod_id == "sodium"
                && r.tier == AdaptationTier::AutoSkipped
                && r.diagnostic.contains(REASON_SUPERSEDED)
        }));
    }

    #[test]
    fn wire_mods_boot_scan_supersedes_loads_culinary_and_classifies() {
        let _serial = WIRE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        fresh_suite();
        let dir = lmb_temp_dir("scan");
        std::fs::write(
            dir.join("sodium-0.6.13.jar"),
            lmb_test_jar(&[("fabric.mod.json", &lmb_fabric_manifest("sodium"))]),
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
        // A superseded-looking filename cannot stand in for loader metadata.
        std::fs::write(dir.join("sodium-0.6.jar"), lmb_test_jar(&[("x.bin", b"x")])).unwrap();
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
        let rejected = suite
            .reports
            .iter()
            .find(|r| r.file_name == "sodium-0.6.jar")
            .expect("manifest-less report");
        assert!(rejected.diagnostic.contains("dependency admission rejected"));
        // Neither the skipped nor the rejected JAR onboards anything.
        assert!(suite.admitted_jars.is_empty());
        assert!(suite.auto_adapted.is_empty());
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
        let _guard = crate::world_guard();
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

        // Advance one server tick so the DistanceTier::Near coalescer's
        // 1-tick cadence allows the next changed observation to emit.
        TICK_IDX.fetch_add(1, Ordering::Relaxed);
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
        live_mod_suite()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .active = true;
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
        assert_eq!(
            lmb_decode_payload(&body),
            ("megu:season".into(), payload.clone())
        );
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
        assert_eq!(
            live_mod_roll_halloween_headgear("minecraft:creeper", 10, 31, 0.01),
            None
        );
        assert_eq!(
            live_mod_roll_halloween_headgear("minecraft:zombie", 10, 30, 0.01),
            None
        );
        assert_eq!(
            live_mod_roll_halloween_headgear("minecraft:zombie", 13, 31, 0.01),
            None
        );
        assert_eq!(
            live_mod_chest_holiday_variant("minecraft:chest", 12, 25),
            "christmas_present"
        );
        assert_eq!(
            live_mod_chest_holiday_variant("minecraft:trapped_chest", 12, 24),
            "christmas_present"
        );
        assert_eq!(
            live_mod_chest_holiday_variant("minecraft:ender_chest", 12, 25),
            "standard"
        );
        assert_eq!(
            live_mod_chest_holiday_variant("minecraft:chest", 12, 27),
            "standard"
        );
        assert_eq!(
            live_mod_try_qol_right_click_harvest(
                true,
                "minecraft:wheat",
                7,
                7,
                "minecraft:wheat",
                "minecraft:wheat_seeds",
                3,
                2
            ),
            Some((0, 3, 1))
        );
        assert_eq!(
            live_mod_try_qol_right_click_harvest(
                true,
                "minecraft:carrots",
                7,
                7,
                "minecraft:carrot",
                "minecraft:carrot",
                3,
                0
            ),
            Some((0, 2, 0))
        );
        assert!(live_mod_try_qol_right_click_harvest(
            true,
            "minecraft:wheat",
            7,
            7,
            "minecraft:wheat",
            "minecraft:wheat_seeds",
            3,
            0
        )
        .is_none());
        assert!(live_mod_try_qol_right_click_harvest(
            false,
            "minecraft:wheat",
            7,
            7,
            "minecraft:wheat",
            "minecraft:wheat_seeds",
            3,
            2
        )
        .is_none());
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
        let _guard = crate::world_guard();
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
        fresh_suite();
    }

    #[test]
    fn wire_mods_progression_denies_break_and_resyncs() {
        let _guard = crate::world_guard();
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
        let _guard = crate::world_guard();
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
        assert_eq!(megu_core::version::TargetVersion::V1_21_1.protocol(), 767);
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
        let _guard = crate::world_guard();
        let _serial = WIRE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        fresh_suite();
        let class = lmb_halve_damage_class();
        assert_eq!(&class[..4], &[0xca, 0xfe, 0xba, 0xbe]);

        // A valid onDamage method alone is not enough to activate a Tier 3 JAR.
        let dir = lmb_temp_dir("damage-class");
        std::fs::write(
            dir.join("damagehook-1.jar"),
            lmb_test_jar(&[
                ("fabric.mod.json", &lmb_fabric_manifest("damagehook")),
                ("example/DamageHook.class", &class),
            ]),
        )
        .unwrap();
        let mut suite = LiveModSuite::new();
        suite.scan_mods_dir(&dir);
        std::fs::remove_dir_all(dir).unwrap();
        assert!(suite.admitted_jars.contains("damagehook-1.jar"));
        assert_eq!(suite.reports[0].tier, AdaptationTier::Tier3FallbackRequired);
        assert!(suite.damage_hooks.is_empty());

        // The exact same bytecode can execute in an admitted Tier 2 JAR.
        lmb_arm_damage_dispatch(&lmb_damage_dispatch_jar(&class));
        assert_eq!(live_mod_eval_damage_hooks(14.0), 7.0);
        assert_eq!(live_mod_eval_damage_hooks(6.0), 6.0);
        assert!(live_mod_eval_damage_hooks(f32::NAN).is_nan());
        fresh_suite();
    }

    // ---------- live EntityDamagePre dispatch ----------

    /// `example/DamageHook.onDamage` bytecode: `event.setDamage(
    /// event.getDamage() * 0.5)` when the hit exceeds 10 — observes and
    /// rewrites, never cancels.
    fn lmb_halve_damage_class() -> Vec<u8> {
        use mod_adapter_core::bytecode_subset_vm::testkit::{ClassAssembler, MethodSpec};
        let mut asm = ClassAssembler::new("example/DamageHook");
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
        asm.finish(&[MethodSpec::new(
            "onDamage",
            "(Lmegu/api/Event;)V",
            4,
            1,
            code,
        )])
    }

    /// `example/CancelDamage.onDamage` bytecode: `event.cancel()`.
    fn lmb_cancel_damage_class() -> Vec<u8> {
        use mod_adapter_core::bytecode_subset_vm::testkit::{ClassAssembler, MethodSpec};
        let mut asm = ClassAssembler::new("example/CancelDamage");
        let cancel = asm.cp.method_ref("megu/api/Event", "cancel", "()V");
        let code = vec![0x2a, 0xb6, (cancel >> 8) as u8, cancel as u8, 0xb1];
        asm.finish(&[MethodSpec::new(
            "onDamage",
            "(Lmegu/api/Event;)V",
            2,
            1,
            code,
        )])
    }

    /// A jar carrying BOTH a liftable `LivingEntity#hurt` HEAD inject (fires
    /// `EntityDamagePre`) and the given verified `onDamage` class — the
    /// combined fixture proving the real production dispatch path, not
    /// just the VM evaluator.
    fn lmb_damage_dispatch_jar(damage_class: &[u8]) -> Vec<u8> {
        let mixin_class = build_class(
            "com/dmgmod/mixin/DamageMixin",
            vec![mixin(vec![(
                "value",
                Ev::Arr(vec![ev_cls("net/minecraft/world/entity/LivingEntity")]),
            )])],
            vec![meth(
                "onHurt",
                vec![ann(
                    INJECT,
                    vec![
                        ("method", ev_s("hurt")),
                        ("at", at("HEAD")),
                        ("cancellable", Ev::Z(true)),
                    ],
                    true,
                )],
            )],
        );
        let config = r#"{"required":true,"package":"com.dmgmod.mixin","mixins":["DamageMixin"]}"#;
        let manifest = lmb_fabric_manifest("dmgmod");
        lmb_test_jar(&[
            ("fabric.mod.json", manifest.as_slice()),
            ("test.mixins.json", config.as_bytes()),
            ("com/dmgmod/mixin/DamageMixin.class", mixin_class.as_slice()),
            ("example/DamageHook.class", damage_class),
        ])
    }

    /// Install a scanned damage-dispatch jar as the armed suite.
    fn lmb_arm_damage_dispatch(jar: &[u8]) {
        let dir = lmb_temp_dir("damage-dispatch");
        std::fs::write(dir.join("dmgmod-1.jar"), jar).unwrap();
        let mut suite = LiveModSuite::new();
        suite.scan_mods_dir(&dir);
        suite.active = true;
        *live_mod_suite().lock().unwrap_or_else(|e| e.into_inner()) = suite;
        live_mod_refresh_hook_gate();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn w_mod_live_damage_pre_passthrough_without_hooks_is_vanilla() {
        let _world = crate::world_guard();
        let _serial = WIRE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        fresh_suite();
        // No suite, no hooks: the fast gate is clear and the amount passes
        // through untouched — identical to a build without mod-suite.
        assert!(!HAS_LIVE_MOD_HOOKS.load(Ordering::Relaxed));
        assert_eq!(live_mod_entity_damage_pre(9.5), Some(9.5));
        let (client, _rx) = crate::effect_tests::client_pair();
        let client = std::sync::Arc::new(client);
        assert!(live_mob_damage_player(
            &client,
            [1.0, 64.0, 1.0],
            10.0,
            None,
            None
        ));
        assert_eq!(*client.hp.lock().unwrap_or_else(|e| e.into_inner()), 10.0);
    }

    #[test]
    fn w_mod_live_damage_pre_binding_and_hook_observe_rewrite_cancel() {
        let _world = crate::world_guard();
        let _serial = WIRE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        fresh_suite();

        // Rewriting handler + EntityDamagePre binding armed.
        lmb_arm_damage_dispatch(&lmb_damage_dispatch_jar(&lmb_halve_damage_class()));
        {
            let suite = live_mod_suite().lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(suite.lifted_hooks.len(), 1);
            assert_eq!(suite.damage_hooks.len(), 1);
        }
        assert!(HAS_LIVE_MOD_HOOKS.load(Ordering::Relaxed));

        // Direct path: binding fires (counter proves dispatch), handler
        // observes 14 -> halves to 7; 6 stays 6.
        assert_eq!(live_mod_entity_damage_pre(14.0), Some(7.0));
        assert_eq!(live_mod_entity_damage_pre(6.0), Some(6.0));
        {
            let suite = live_mod_suite().lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(
                suite.hook_fires.get(&StandardEventHook::EntityDamagePre),
                Some(&2)
            );
        }

        // Real damage path: a 14-damage mob hit lands as 7 hp damage.
        let (client, _rx) = crate::effect_tests::client_pair();
        let client = std::sync::Arc::new(client);
        assert!(live_mob_damage_player(
            &client,
            [1.0, 64.0, 1.0],
            14.0,
            None,
            None
        ));
        assert_eq!(*client.hp.lock().unwrap_or_else(|e| e.into_inner()), 13.0);
        {
            let suite = live_mod_suite().lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(
                suite.hook_fires.get(&StandardEventHook::EntityDamagePre),
                Some(&3)
            );
        }

        // Cancelling handler: same dispatch path returns None and the real
        // damage path refuses the hit outright — hp untouched.
        lmb_arm_damage_dispatch(&lmb_damage_dispatch_jar(&lmb_cancel_damage_class()));
        assert_eq!(live_mod_entity_damage_pre(9.0), None);
        let (client, _rx) = crate::effect_tests::client_pair();
        let client = std::sync::Arc::new(client);
        assert!(!live_mob_damage_player(
            &client,
            [1.0, 64.0, 1.0],
            9.0,
            None,
            None
        ));
        assert_eq!(*client.hp.lock().unwrap_or_else(|e| e.into_inner()), 20.0);

        fresh_suite();
    }

    #[test]
    fn live_mod_bridge_vanilla_player_has_no_magic_sync() {
        use super::live_mod_bridge_magic_tests::{reset_magic, MAGIC_TEST_LOCK};

        let _world = crate::world_guard();
        let _serial = WIRE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _magic = MAGIC_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        fresh_suite();
        reset_magic();

        let (client, mut peer) = lmb_client("vanilla_magic_probe", 977_305);
        clients()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Arc::clone(&client));
        live_mod_tick();
        let no_initial_frame = lmb_silent(&mut peer);
        let no_mana_pool = !live_mod_magic_remove_player(client.uuid);

        live_mod_magic_register_status(lmb_magic::StatusEffectDefinition {
            id: "test:ward".into(),
            ..lmb_magic::StatusEffectDefinition::default()
        })
        .unwrap();
        live_mod_tick();
        let frame = lmb_read_frame(&mut peer, &mut Vec::new());
        clients()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|player| player.eid != client.eid);
        reset_magic();

        assert!(no_initial_frame, "vanilla tick emitted a packet");
        assert!(no_mana_pool, "vanilla tick initialized a ManaPool");
        let (id, body) = frame.expect("configured magic sends an initial snapshot");
        assert_eq!(id, LIVE_MOD_S2C_CUSTOM_PAYLOAD_ID);
        let (channel, body) = lmb_decode_payload(&body);
        assert_eq!(channel, lmb_magic::MAGIC_CHANNEL);
        assert_eq!(lmb_magic::decode_magic_sync(&body).unwrap().max_mana, 100.0);
    }

    #[test]
    fn w_mod_live_tick_hooks_fire_on_server_tick_path() {
        let _world = crate::world_guard();
        let _serial = WIRE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        fresh_suite();

        // Zero hooks: the real tick path dispatches nothing.
        live_mod_tick();
        {
            let suite = live_mod_suite().lock().unwrap_or_else(|e| e.into_inner());
            assert!(suite.hook_fires.is_empty());
        }

        // A liftable `ServerLevel#tick` HEAD inject binds ServerTickPre;
        // the production `live_mod_tick` must actually dispatch it.
        let dir = lmb_temp_dir("tick-dispatch");
        std::fs::write(
            dir.join("tickmod-1.jar"),
            lmb_inject_jar(
                "com/tickmod",
                "net/minecraft/server/level/ServerLevel",
                "tick",
            ),
        )
        .unwrap();
        let mut suite = LiveModSuite::new();
        suite.scan_mods_dir(&dir);
        suite.active = true;
        *live_mod_suite().lock().unwrap_or_else(|e| e.into_inner()) = suite;
        live_mod_refresh_hook_gate();
        let _ = std::fs::remove_dir_all(dir);

        live_mod_tick();
        {
            let suite = live_mod_suite().lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(
                suite.hook_fires.get(&StandardEventHook::ServerTickPre),
                Some(&1),
                "registered tick binding must fire on the real tick path"
            );
        }
        fresh_suite();
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
        live_mod_refresh_hook_gate();

        let key = (DimId::Overworld, 10, 64, -3);
        let mut pot = CookingPotState::new();
        assert!(pot.insert_input("minecraft:carrot", 1));
        pot.container = Some(("minecraft:bowl".into(), 1));
        live_mod_set_cooking_pot(key.0.raw(), key.1, key.2, key.3, pot.clone(), 100);
        {
            let suite = live_mod_suite().lock().unwrap_or_else(|e| e.into_inner());
            assert!(suite.cooking_pots[&key]
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pot()
                .is_sleeping());
            assert!(suite.cooking_deadlines.is_empty());
        }
        assert!(live_mod_tick_cooking_pots(100_000).is_empty());
        pot.heated = true;
        live_mod_set_cooking_pot(key.0.raw(), key.1, key.2, key.3, pot, 100_000);
        assert!(live_mod_tick_cooking_pots(100_199).is_empty());
        let completed = live_mod_tick_cooking_pots(100_200);
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].0, key);
        assert_eq!(completed[0].1.result_item, "farmersdelight:test_soup");
        assert!(live_mod_tick_cooking_pots(100_201).is_empty());
        let cut = live_mod_cutting_board_use("minecraft:carrot", "unknownaddon:knife", 42).unwrap();
        assert_eq!(cut.outputs, [("unknownaddon:carrot_slices".to_string(), 3)]);
        live_mod_remove_cooking_pot(key.0.raw(), key.1, key.2, key.3);
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

        let _guard = crate::world_guard();
        let _serial = WIRE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        fresh_suite();
        *live_magic_state().lock().unwrap_or_else(|e| e.into_inner()) = LiveMagicState::default();
        *dimension_mod_bridge()
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = DimensionModRuntimeBridge::new();

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
        live_mod_refresh_hook_gate();

        // Heated pot with carrot + bowl completes the scanned recipe on its deadline.
        let key = (DimId::Overworld, 30, 64, 30);
        let mut pot = CookingPotState::new();
        assert!(pot.insert_input("minecraft:carrot", 1));
        pot.container = Some(("minecraft:bowl".into(), 1));
        pot.heated = true;
        live_mod_set_cooking_pot(key.0.raw(), key.1, key.2, key.3, pot, 500_000);
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
        live_mod_remove_cooking_pot(key.0.raw(), key.1, key.2, key.3);

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
                true,
                "minecraft:wheat",
                7,
                7,
                "minecraft:wheat",
                "minecraft:wheat_seeds",
                3,
                2
            ),
            Some((0, 3, 1))
        );
        assert!(live_mod_try_qol_right_click_harvest(
            true,
            "minecraft:wheat",
            7,
            7,
            "minecraft:wheat",
            "minecraft:wheat_seeds",
            3,
            0
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
            Err(LiveMagicError::Cast(
                lmb_magic::CastError::OnCooldown { .. }
            ))
        ));
        let tick = live_mod_magic_tick_player(uuid).unwrap();
        let frame = tick.frame.expect("MAG1 sync frame");
        assert!(frame.windows(4).any(|w| w == b"MAG1"));

        // ---- dimension portal: transition -> intent -> anti-cheat grace ----
        {
            let mut b = dimension_mod_bridge()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
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
            let b = dimension_mod_bridge()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
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
        let to = (
            intent.target_pos[0],
            intent.target_pos[1],
            intent.target_pos[2],
        );
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
        *dimension_mod_bridge()
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = DimensionModRuntimeBridge::new();
        *live_magic_state().lock().unwrap_or_else(|e| e.into_inner()) = LiveMagicState::default();
        fresh_suite();
        let _ = std::fs::remove_dir_all(dir);
    }

    // ---------- MRS1 dynamic registry sync ----------

    /// Vanilla / unarmed: `live_mod_registry_sync_frame` must stay `None`
    /// and the join hook must put zero bytes on the wire.
    #[test]
    fn wire_mods_mrs1_vanilla_emits_nothing() {
        // Serializes against `live_create_tests`' world_guard too — this
        // test mutates LIVE_CREATE_TEST_GATE, which the armed runtime reads.
        let _g = world_guard();
        let _serial = WIRE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        fresh_suite();
        LIVE_CREATE_TEST_GATE.store(2, Ordering::Relaxed);
        assert!(
            live_mod_registry_sync_frame().is_none(),
            "unarmed server must not emit an MRS1 frame"
        );
        let (mut send, mut peer) = lmb_socket_pair();
        live_mod_registry_sync_on_join(&mut send);
        assert!(
            lmb_silent(&mut peer),
            "vanilla join must emit no MRS1 packet"
        );
        LIVE_CREATE_TEST_GATE.store(1, Ordering::Relaxed);
    }

    /// Create armed: the frame decodes under the client contract — `MRS1`
    /// magic, IEEE CRC32 and monotonic ids are all enforced by
    /// `decode_registry_sync_frame` — and carries the `create:*` item table.
    /// When `breg()` is loaded with the mod extension, block states join too.
    #[test]
    fn wire_mods_mrs1_create_armed_frame_decodes() {
        // Serializes against `live_create_tests`' world_guard too — this
        // test mutates LIVE_CREATE_TEST_GATE, which the armed runtime reads.
        let _g = world_guard();
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
        if breg().and_then(|reg| reg.create_registry()).is_some() {
            assert!(
                synced
                    .block_states
                    .iter()
                    .any(|e| e.block_id == "create:shaft"),
                "armed Create registry must sync its block states"
            );
            assert!(synced
                .block_states
                .iter()
                .all(|e| { e.state_id >= create_adapter::blockstates::DEFAULT_BASE_STATE_ID }));
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
        assert_eq!(payload.as_slice(), &frame[..]);
    }

    /// The encoded `MRS1` frame is cached per registry revision: repeated
    /// joins get the same `Arc` (zero re-encode), and a rescan — which
    /// bumps `registry_revision` — forces exactly one rebuild that still
    /// produces byte-identical bytes for unchanged inputs.
    #[test]
    fn wire_mods_mrs1_frame_is_cached_and_rescan_rebuilds() {
        // Serializes against `live_create_tests`' world_guard too — this
        // test mutates LIVE_CREATE_TEST_GATE, which the armed runtime reads.
        let _g = world_guard();
        let _serial = WIRE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        fresh_suite();
        LIVE_CREATE_TEST_GATE.store(1, Ordering::Relaxed);
        let first = live_mod_registry_sync_frame().expect("armed Create must emit MRS1");
        let second = live_mod_registry_sync_frame().expect("cache hit must emit MRS1");
        assert!(
            Arc::ptr_eq(&first, &second),
            "repeat joins must share one encoded frame"
        );

        // A real rescan of a scanned dir is the only production
        // invalidation path — drive it rather than poking fields.
        let dir = lmb_temp_dir("mrs1-rescan");
        {
            let mut suite = live_mod_suite().lock().unwrap_or_else(|e| e.into_inner());
            suite.scan_mods_dir(&dir);
        }
        let third = live_mod_registry_sync_frame().expect("post-rescan join must emit MRS1");
        assert!(
            !Arc::ptr_eq(&first, &third),
            "registry revision bump must rebuild the frame"
        );
        assert_eq!(
            &first[..],
            &third[..],
            "unchanged inputs must rebuild byte-identical bytes"
        );
        let fourth = live_mod_registry_sync_frame().expect("second post-rescan join");
        assert!(
            Arc::ptr_eq(&third, &fourth),
            "rebuilt frame must cache again"
        );

        let _ = std::fs::remove_dir_all(&dir);
        fresh_suite();
        LIVE_CREATE_TEST_GATE.store(1, Ordering::Relaxed);
    }

    /// `MEGU_MODS_DIR` content: a scanned JAR's declared blockstates/items
    /// land in the suite's registry plan and ride the same MRS1 frame at the
    /// dedicated generic id bases — disjoint from Create's ranges.
    #[test]
    fn wire_mods_mrs1_includes_mods_dir_registry_plan() {
        // Serializes against `live_create_tests`' world_guard too — this
        // test mutates LIVE_CREATE_TEST_GATE, which the armed runtime reads.
        let _g = world_guard();
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
        live_mod_refresh_hook_gate();

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
            synced
                .items
                .keys()
                .all(|id| *id >= LIVE_MRS_GENERIC_ITEM_BASE),
            "generic item ids must stay in their dedicated range"
        );

        let _ = std::fs::remove_dir_all(&dir);
        fresh_suite();
        LIVE_CREATE_TEST_GATE.store(1, Ordering::Relaxed);
    }

    /// Every block the JAR block table extracted must resolve through the
    /// live lookup — `/fill`/`/clone` (`modification_block_state`),
    /// `/setblock` (`state_from_arg`), and the state→name reverse mapping.
    #[test]
    fn wire_mods_generic_blocks_resolve_for_fill_setblock() {
        // Serializes against `live_create_tests`' world_guard too — this
        // test mutates LIVE_CREATE_TEST_GATE, which the armed runtime reads.
        let _g = world_guard();
        let _serial = WIRE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        fresh_suite();
        LIVE_CREATE_TEST_GATE.store(2, Ordering::Relaxed);
        let dir = lmb_temp_dir("generic-blocks");
        let blockstates = br#"{"variants":{"axis=x,waterlogged=false":{"model":"testmod:block/rock"},"axis=y,waterlogged=false":{"model":"testmod:block/rock"},"axis=x,waterlogged=true":{"model":"testmod:block/rock"},"axis=y,waterlogged=true":{"model":"testmod:block/rock"}}}"#;
        let manifest = lmb_fabric_manifest("testmod");
        let jar = lmb_test_jar(&[
            ("fabric.mod.json", manifest.as_slice()),
            ("assets/testmod/blockstates/rock.json", blockstates),
        ]);
        std::fs::write(dir.join("testmod.jar"), &jar).unwrap();
        let mut suite = LiveModSuite::new();
        suite.scan_mods_dir(&dir);
        *live_mod_suite().lock().unwrap_or_else(|e| e.into_inner()) = suite;
        live_mod_refresh_hook_gate();

        // Partial-property selectors overlay the default state, exactly
        // like `BReg::checked_state` does for vanilla ids.
        let rock_y =
            live_mod_state_for_name("testmod:rock", &[("axis".to_string(), "y".to_string())])
                .expect("testmod:rock y-axis state resolves");
        assert!(rock_y >= LIVE_MRS_GENERIC_BLOCK_BASE);
        assert_eq!(
            live_mod_name_for_state(rock_y).as_deref(),
            Some("testmod:rock")
        );

        // `/fill` resolves through `modification_block_state`; `/setblock`
        // through `state_from_arg` — both must answer the same states.
        assert_eq!(
            modification_block_state("testmod:rock[axis=y]"),
            Some(rock_y)
        );
        assert_eq!(state_from_arg("testmod:rock[axis=y]"), Some(rock_y));
        let rock_default =
            live_mod_state_for_name("testmod:rock", &[]).expect("default rock state");
        assert_eq!(state_from_arg("testmod:rock"), Some(rock_default));
        assert_eq!(modification_block_state("testmod:rock"), Some(rock_default));

        // Bad properties and unknown blocks still fail closed; vanilla and
        // unnamespaced names never consult the mod tables.
        assert_eq!(modification_block_state("testmod:rock[axis=q]"), None);
        assert_eq!(modification_block_state("testmod:missing"), None);
        assert_eq!(live_mod_state_for_arg("minecraft:stone"), None);
        assert_eq!(live_mod_state_for_arg("stone"), None);

        let _ = std::fs::remove_dir_all(&dir);
        fresh_suite();
        LIVE_CREATE_TEST_GATE.store(1, Ordering::Relaxed);
    }

    /// MCF2 must serialize a plan-table (non-core-catalog) state under a
    /// resolvable canonical name — never `""` — and the decoder must hand
    /// the same state id back. Before the fix, `encode_canonical` wrote an
    /// empty string for such states and the reload failed closed on the
    /// whole sidecar.
    #[test]
    fn wire_mods_sidecar_plan_state_survives_roundtrip() {
        // Serializes against `live_create_tests`' world_guard too — this
        // test mutates LIVE_CREATE_TEST_GATE, which the armed runtime reads.
        let _world = crate::world_guard();
        let _serial = WIRE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        fresh_suite();
        LIVE_CREATE_TEST_GATE.store(1, Ordering::Relaxed);
        *live_create_state()
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;

        // A minimal plan with one custom block — no real JAR needed.
        let mut suite = LiveModSuite::new();
        suite.registry_plan = Some(UnifiedModRegistryPlan {
            base_block_state_id: LIVE_MRS_GENERIC_BLOCK_BASE,
            base_item_id: LIVE_MRS_GENERIC_ITEM_BASE,
            block_states: vec![
                DynamicBlockStateEntry {
                    state_id: LIVE_MRS_GENERIC_BLOCK_BASE,
                    block_id: "mymod:custom_block".to_string(),
                    properties: vec![("facing".to_string(), "north".to_string())],
                    facts_override: None,
                },
                DynamicBlockStateEntry {
                    state_id: LIVE_MRS_GENERIC_BLOCK_BASE + 1,
                    block_id: "mymod:custom_block".to_string(),
                    properties: vec![("facing".to_string(), "south".to_string())],
                    facts_override: None,
                },
            ],
            items: BTreeMap::new(),
            content: RegistryContentIndex::default(),
            bytecode_facts: BTreeMap::new(),
        });
        *live_mod_suite().lock().unwrap_or_else(|e| e.into_inner()) = suite;
        live_mod_refresh_hook_gate();

        // A tracked cell holding the plan state serializes under the
        // JAR-table canonical name and the decoder returns the same id.
        let pos = FactoryPos::new(DimId::Overworld.raw() as i32, 4_000_001, 80, 4_000_001);
        live_create_with(|rt| {
            rt.placed.insert(pos, LIVE_MRS_GENERIC_BLOCK_BASE + 1);
        });
        let blob = live_create_serialize().expect("armed runtime serializes");
        let entries = live_create_decode_sidecar(&blob).expect("plan state decodes");
        assert_eq!(
            entries
                .iter()
                .map(|(p, st, _)| (*p, *st))
                .collect::<Vec<_>>(),
            vec![(pos, LIVE_MRS_GENERIC_BLOCK_BASE + 1)]
        );

        // The truly-unresolvable fallback stays non-empty and decodes to
        // the raw id rather than corrupting the file.
        let registry = create_adapter::blockstates::core_create_registry(
            create_adapter::blockstates::DEFAULT_BASE_STATE_ID,
        );
        let unresolvable = u32::MAX - 7;
        assert!(registry.state(unresolvable).is_none());
        let fallback = live_create_encode_state(&registry, unresolvable);
        assert_eq!(
            fallback,
            format!("megu:unresolved_block_state[{unresolvable}]")
        );
        assert_eq!(live_create_unresolved_state(&fallback), Some(unresolvable));

        fresh_suite();
        *live_create_state()
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        LIVE_CREATE_TEST_GATE.store(0, Ordering::Relaxed);
    }

    // ---------- XMOD1: unified tags + shared cross-mod capability bus ----------

    /// Synthetic-JAR nested tags: `#c:` and `#forge:`/`#neoforge:`
    /// spellings, Jar-in-Jar members and the vanilla baseline all resolve
    /// through ONE index, and the culinary `TagIndex` picks up the
    /// flattened export on both spellings.
    #[test]
    fn wire_mods_xmod1_unified_tags_nested_cross_mod() {
        let _g = world_guard();
        let _serial = WIRE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        fresh_suite();
        let dir = lmb_temp_dir("xmod1-tags");
        // Mod A: `c:ingots` nested-references the shared
        // `#forge:ingots/iron` subtag — the index resolves the chain
        // uniformly across conventions.
        std::fs::write(
            dir.join("mod_a.jar"),
            lmb_test_jar(&[
                ("fabric.mod.json", lmb_fabric_manifest("mod_a").as_slice()),
                (
                    "data/c/tags/item/ingots.json",
                    br##"{"values":["#forge:ingots/iron"]}"##.as_slice(),
                ),
            ]),
        )
        .unwrap();
        // Mod B contributes the `forge:` subtag plus a `c:plates/brass`
        // member, and nests an inner JAR whose `c:` tag must fold in
        // through the bounded Jar-in-Jar recursion.
        let inner = lmb_test_jar(&[(
            "data/c/tags/item/gems/focus.json",
            br#"{"values":["mod_inner:focus_gem"]}"#.as_slice(),
        )]);
        std::fs::write(
            dir.join("mod_b.jar"),
            lmb_test_jar(&[
                ("fabric.mod.json", lmb_fabric_manifest("mod_b").as_slice()),
                (
                    "data/forge/tags/item/ingots/iron.json",
                    br#"{"values":["mod_b:steel_ingot","minecraft:iron_ingot"]}"#.as_slice(),
                ),
                (
                    "data/c/tags/item/plates/brass.json",
                    br#"{"values":["mod_b:brass_plate"]}"#.as_slice(),
                ),
                ("META-INF/jarjar/inner.jar", inner.as_slice()),
            ]),
        )
        .unwrap();
        let mut suite = LiveModSuite::new();
        suite.scan_mods_dir(&dir);
        *live_mod_suite().lock().unwrap_or_else(|e| e.into_inner()) = suite;
        live_mod_refresh_hook_gate();

        // `c:`, `forge:` and `neoforge:` spellings hit the same canonical
        // chain.
        for tag in [
            "#c:ingots/iron",
            "#forge:ingots/iron",
            "#neoforge:ingots/iron",
        ] {
            assert!(
                live_unified_tag_matches(TagDomain::Item, tag, "mod_b:steel_ingot"),
                "{tag} must match the cross-mod member"
            );
        }
        // Nested: `c:ingots` -> `#forge:ingots/iron`.
        assert!(live_unified_tag_matches(
            TagDomain::Item,
            "#c:ingots",
            "mod_b:steel_ingot"
        ));
        assert!(live_unified_tag_matches(
            TagDomain::Item,
            "#forge:plates/brass",
            "mod_b:brass_plate"
        ));
        // Jar-in-Jar member tags merged too.
        assert!(live_unified_tag_matches(
            TagDomain::Item,
            "#c:gems/focus",
            "mod_inner:focus_gem"
        ));
        // The vanilla 1.21.1 `c:` baseline still applies with mods loaded.
        assert!(live_unified_tag_matches(
            TagDomain::Item,
            "#c:crops",
            "minecraft:carrot"
        ));
        assert!(!live_unified_tag_matches(
            TagDomain::Item,
            "#c:ingots",
            "minecraft:stone"
        ));
        // The culinary flat `TagIndex` picked up the flattened export on
        // both spellings — `#` chains pre-expanded.
        {
            let suite = live_mod_suite().lock().unwrap_or_else(|e| e.into_inner());
            assert!(mod_adapter_core::culinary_adapter::tag_contains(
                &suite.culinary.tags,
                "c:ingots/iron",
                "mod_b:steel_ingot"
            ));
            assert!(mod_adapter_core::culinary_adapter::tag_contains(
                &suite.culinary.tags,
                "forge:ingots/iron",
                "mod_b:steel_ingot"
            ));
        }
        let _ = std::fs::remove_dir_all(dir);
        fresh_suite();
    }

    /// Mod A item carrying 1.21.1 `raw_components` bytes rides a Create
    /// chute's bus endpoint into a foreign `SimpleSlotInventory` one bus
    /// step later — byte-identical on extraction, in the Overworld, the
    /// Nether and an unregistered custom `DimId`.
    #[test]
    fn wire_mods_xmod1_cap_bus_item_components_roundtrip() {
        let _g = world_guard();
        let _serial = WIRE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        fresh_suite();
        LIVE_CREATE_TEST_GATE.store(1, Ordering::Relaxed);
        *live_create_state()
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        live_cap_reset();

        let reg = live_create_block_registry().expect("armed registry");
        let chute_st = reg.default_state("create:chute").expect("create:chute");
        for (dim, (x, y, z)) in [
            (DimId::Overworld, (200, 70, 200)),
            (DimId::Nether, (210, 70, 210)),
            (DimId::from_raw(7), (220, 70, 220)),
        ] {
            live_create_on_block_set(dim, x, y, z, 0, chute_st);
            // Mod B storage cell directly below the chute.
            live_cap_register_slot_inventory(dim, x, y - 1, z, 4, 64);
            let comps = vec![0xDE, 0xAD, 0xBE, 0xEF, x as u8];
            assert_eq!(
                live_cap_insert_item(
                    dim,
                    x,
                    y,
                    z,
                    Some(FaceDirection::Up),
                    "mod_a:focus_gem",
                    3,
                    &comps,
                    false
                ),
                3,
                "chute endpoint accepted the whole stack"
            );
            // A simulate probe leaves the stack in place.
            assert_eq!(
                live_cap_insert_item(
                    dim,
                    x,
                    y,
                    z,
                    Some(FaceDirection::Up),
                    "mod_a:focus_gem",
                    1,
                    &comps,
                    true
                ),
                1
            );
            live_cap_bus_step();
            let out = live_cap_extract_item(dim, x, y - 1, z, Some(FaceDirection::Up), 64, false)
                .expect("mod B storage received the pushed stack");
            assert_eq!(out.item_id, "mod_a:focus_gem");
            assert_eq!(out.count, 3, "zero duplication/loss across the hop");
            assert_eq!(
                out.raw_components, comps,
                "1.21.1 components must survive byte-identical"
            );
            assert!(
                live_cap_extract_item(dim, x, y, z, None, 64, false).is_none(),
                "the chute's endpoint is drained after the push"
            );
            live_cap_unregister(dim, x, y - 1, z);
            live_create_on_block_set(dim, x, y, z, chute_st, 0);
        }
        LIVE_CREATE_TEST_GATE.store(0, Ordering::Relaxed);
        fresh_suite();
    }

    /// The full cross-mod story with zero glue: a Create chute pushes a
    /// component-tagged ingredient straight into a culinary cooking pot's
    /// canonical state, the deadline scheduler cooks it, and the finished
    /// meal is served back out through the same bus.
    #[test]
    fn wire_mods_xmod1_cap_bus_chute_feeds_cooking_pot() {
        let _g = world_guard();
        let _serial = WIRE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        fresh_suite();
        LIVE_CREATE_TEST_GATE.store(1, Ordering::Relaxed);
        *live_create_state()
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        live_cap_reset();

        let dir = lmb_temp_dir("xmod1-pot");
        std::fs::write(
            dir.join("farmersdelight.jar"),
            lmb_culinary_jar("farmersdelight"),
        )
        .unwrap();
        let mut suite = LiveModSuite::new();
        suite.scan_mods_dir(&dir);
        suite.active = true;
        *live_mod_suite().lock().unwrap_or_else(|e| e.into_inner()) = suite;
        live_mod_refresh_hook_gate();

        let reg = live_create_block_registry().expect("armed registry");
        let chute_st = reg.default_state("create:chute").expect("create:chute");
        let (px, py, pz) = (300, 64, 300);
        let mut pot = CookingPotState::new();
        pot.heated = true;
        pot.container = Some(("minecraft:bowl".into(), 1));
        TICK_IDX.store(0, Ordering::Relaxed);
        live_mod_set_cooking_pot(DimId::Overworld.raw(), px, py, pz, pot, 0);
        live_create_on_block_set(DimId::Overworld, px, py + 1, pz, 0, chute_st);

        // A carrot with component bytes enters the chute; one bus step
        // delivers it into the SAME adapter the deadline scheduler owns.
        let comps = vec![0x0C, 0xA0];
        assert_eq!(
            live_cap_insert_item(
                DimId::Overworld,
                px,
                py + 1,
                pz,
                None,
                "minecraft:carrot",
                1,
                &comps,
                false
            ),
            1
        );
        live_cap_bus_step();
        let adapter = {
            let suite = live_mod_suite().lock().unwrap_or_else(|e| e.into_inner());
            suite.cooking_pots[&(DimId::Overworld, px, py, pz)].clone()
        };
        {
            let guard = adapter.lock().unwrap_or_else(|e| e.into_inner());
            assert!(guard.pot().inputs.iter().any(|s| {
                s.as_ref()
                    .is_some_and(|(id, c)| id == "minecraft:carrot" && *c == 1)
            }));
            // The bus-fed ingredient armed the cook deadline — the pot is
            // no longer sleeping on its 200-tick recipe.
            assert!(guard.pot().next_completion_tick.is_some());
        }
        let completed = live_mod_tick_cooking_pots(200);
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].1.result_item, "farmersdelight:test_soup");
        // …and the meal comes back out through the pot's Down face on the
        // very same bus.
        let meal = live_cap_extract_item(
            DimId::Overworld,
            px,
            py,
            pz,
            Some(FaceDirection::Down),
            1,
            false,
        )
        .expect("the cooked meal is served through the bus");
        assert_eq!(meal.item_id, "farmersdelight:test_soup");
        assert_eq!(meal.count, 1);

        live_mod_remove_cooking_pot(DimId::Overworld.raw(), px, py, pz);
        live_create_on_block_set(DimId::Overworld, px, py + 1, pz, chute_st, 0);
        LIVE_CREATE_TEST_GATE.store(0, Ordering::Relaxed);
        let _ = std::fs::remove_dir_all(dir);
        fresh_suite();
    }

    /// The `live_create_duct_insert` entry path: a component-free bus
    /// insert into a depot lands in the live `held` slot (and pulls back
    /// out through the bus), and the same insert into a chute rides the
    /// transport FIFO until a bus step ejects it into the foreign
    /// endpoint below — zero duplication, zero loss.
    #[test]
    fn wire_mods_xmod1_cap_bus_duct_insert_rides_transport() {
        let _g = world_guard();
        let _serial = WIRE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        fresh_suite();
        LIVE_CREATE_TEST_GATE.store(1, Ordering::Relaxed);
        *live_create_state()
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        live_cap_reset();
        let reg = live_create_block_registry().expect("armed registry");
        let chute_st = reg.default_state("create:chute").expect("create:chute");
        let depot_st = reg.default_state("create:depot").expect("create:depot");
        let dim = DimId::Overworld;
        let (x, y, z) = (600, 70, 600);

        // Depot: the hopper-style insert lands in the live `held` slot.
        live_create_on_block_set(dim, x, y, z, 0, depot_st);
        assert_eq!(
            live_cap_insert_item(dim, x, y, z, None, "minecraft:carrot", 1, &[], false),
            1
        );
        {
            let guard = live_create_state()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let rt = guard.as_ref().expect("runtime armed");
            let held = rt
                .held
                .get(&FactoryPos::new(0, x, y, z))
                .expect("depot held slot populated");
            assert_eq!(held.items.len(), 1);
        }
        // …and the same item pulls back out through the bus.
        let pulled = live_cap_extract_item(dim, x, y, z, None, 64, false)
            .expect("depot held slot serves bus pulls");
        assert_eq!(pulled.item_id, "minecraft:carrot");
        assert_eq!(pulled.count, 1);

        // Chute: the insert enters the transport machine FIFO; one
        // immediate bus step finds nothing staged in output yet.
        live_create_on_block_set(dim, x, y + 2, z, 0, chute_st);
        live_cap_register_slot_inventory(dim, x, y + 1, z, 4, 64);
        assert_eq!(
            live_cap_insert_item(dim, x, y + 2, z, None, "minecraft:wheat", 1, &[], false),
            1
        );
        live_cap_bus_step();
        assert!(
            live_cap_extract_item(dim, x, y + 1, z, Some(FaceDirection::Up), 64, false).is_none()
        );
        // Advance the transport clock past the chute's staging delay —
        // the staged output then hops into the foreign endpoint on the
        // next bus step.
        let base = TICK_IDX.load(Ordering::Relaxed);
        let mut delivered = None;
        for i in 1..=16u64 {
            TICK_IDX.store(base + i, Ordering::Relaxed);
            let _ = live_create_tick_body();
            live_cap_bus_step();
            if let Some(stack) =
                live_cap_extract_item(dim, x, y + 1, z, Some(FaceDirection::Up), 64, false)
            {
                delivered = Some(stack);
                break;
            }
        }
        let out = delivered.expect("wheat reached the foreign endpoint through the duct");
        assert_eq!(out.item_id, "minecraft:wheat");
        assert_eq!(out.count, 1);

        live_cap_unregister(dim, x, y + 1, z);
        live_create_on_block_set(dim, x, y + 2, z, chute_st, 0);
        live_create_on_block_set(dim, x, y, z, depot_st, 0);
        LIVE_CREATE_TEST_GATE.store(0, Ordering::Relaxed);
        fresh_suite();
    }

    /// Cross-mod fluid + FE energy over the shared bus: a Mod A tank's
    /// molten zinc (component bytes in tow) moves into a Mod B tank, and
    /// FE hops between two generic buffers — all atomically.
    #[test]
    fn wire_mods_xmod1_cap_bus_fluid_energy_cross_mod() {
        let _g = world_guard();
        let _serial = WIRE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        fresh_suite();
        let dim = DimId::Nether;
        let (ax, ay, az) = (400, 70, 400);
        let (bx, by, bz) = (401, 70, 400);
        live_cap_register_fluid_tank(dim, ax, ay, az, 8_000);
        live_cap_register_fluid_tank(dim, bx, by, bz, 16_000);
        assert_eq!(
            live_cap_fill_fluid(
                dim,
                ax,
                ay,
                az,
                "mod_a:molten_zinc",
                3_000,
                &[7, 7, 7],
                false
            ),
            3_000
        );
        assert_eq!(
            live_cap_transfer_fluid(
                dim,
                [ax, ay, az],
                FaceDirection::East,
                [bx, by, bz],
                FaceDirection::West,
                2_000
            ),
            2_000
        );
        let drained =
            live_cap_drain_fluid(dim, bx, by, bz, 4_000, false).expect("mod B tank content");
        assert_eq!(drained.fluid_id, "mod_a:molten_zinc");
        assert_eq!(drained.amount_mb, 2_000);
        assert_eq!(drained.raw_components, vec![7, 7, 7]);

        live_cap_register_energy_buffer(dim, ax, ay, az, 10_000);
        live_cap_register_energy_buffer(dim, bx, by, bz, 10_000);
        assert_eq!(
            live_cap_receive_energy(dim, ax, ay, az, 5_000, false),
            5_000
        );
        assert_eq!(
            live_cap_transfer_energy(
                dim,
                [ax, ay, az],
                FaceDirection::East,
                [bx, by, bz],
                FaceDirection::West,
                4_000
            ),
            4_000
        );
        live_cap_unregister(dim, ax, ay, az);
        live_cap_unregister(dim, bx, by, bz);
        fresh_suite();
    }

    /// A declarative `ModOffloadProfile` block drops its declared
    /// endpoints straight onto the live bus — no mod-specific glue.
    #[cfg(feature = "create-offload")]
    #[test]
    fn wire_mods_xmod1_cap_bus_profile_endpoint() {
        let _g = world_guard();
        let _serial = WIRE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        fresh_suite();
        // Stage the Mekanism-class reference profile on the shared bridge,
        // restoring whatever was loaded after the test.
        let saved = {
            let mut bridge = mod_offload_bridge()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            std::mem::replace(
                &mut bridge.profiles,
                vec![mod_adapter_core::profile::mekanism_class_profile()],
            )
        };
        let dim = DimId::Overworld;
        let (x, y, z) = (500, 70, 500);
        assert!(live_cap_register_profile_block(
            dim,
            x,
            y,
            z,
            "mekanism:basic_fluid_tank"
        ));
        // Mekanism's basic tank declares 16 000 mB.
        assert_eq!(
            live_cap_fill_fluid(dim, x, y, z, "mekanism:water", 16_000, &[], false),
            16_000
        );
        assert_eq!(
            live_cap_fill_fluid(dim, x, y, z, "mekanism:water", 1, &[], false),
            0,
            "a full tank accepts nothing more"
        );
        let drained = live_cap_drain_fluid(dim, x, y, z, 16_000, false).expect("tank content");
        assert_eq!(drained.fluid_id, "mekanism:water");
        assert_eq!(drained.amount_mb, 16_000);
        // A block id no profile declares registers nothing.
        assert!(!live_cap_register_profile_block(
            dim,
            x,
            y,
            z + 1,
            "mekanism:unknown_block"
        ));
        live_cap_unregister(dim, x, y, z);
        mod_offload_bridge()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .profiles = saved;
        fresh_suite();
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
        *live_create_state()
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        // A rebuilt runtime owns no capability endpoints — drop the stale
        // bus state the previous runtime left registered.
        live_cap_reset();
        // The auto-optimizer's cadence/memo tables are process-global —
        // a stale backoff entry at a reused cell could suppress this
        // test's first polls.
        live_auto_optimizer_reset();
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
    const W8: (i32, i32, i32) = (9_000_000, 80, 700);

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
        // The canonical belt block is `create:belt`; the legacy
        // `create:mechanical_belt` item stays accepted as an alias.
        let belt = live_create_resolve_item("create:belt").expect("belt item");
        assert_eq!(live_create_item_block_name(belt), Some("create:belt"));
        let legacy = live_create_resolve_item("create:mechanical_belt").expect("legacy belt item");
        assert_eq!(live_create_item_block_name(legacy), Some("create:belt"));
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
            let id =
                live_create_resolve_item(item).unwrap_or_else(|| panic!("missing item for {item}"));
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
    fn create_belt_alias_resolves_and_creative_motor_spins() {
        let _g = world_guard();
        arm();
        let reg = registry();
        // Legacy `create:mechanical_belt` spellings resolve to the same
        // states as canonical `create:belt` through every lookup surface.
        let belt = state_of("create:belt", &[("facing", "north")]);
        assert_eq!(
            reg.default_state("create:mechanical_belt"),
            reg.default_state("create:belt")
        );
        assert_eq!(
            live_mod_state_for_name(
                "create:mechanical_belt",
                &[("facing".to_string(), "north".to_string())]
            ),
            Some(belt)
        );
        assert_eq!(
            live_mod_state_for_arg("create:mechanical_belt[facing=north]"),
            Some(belt)
        );
        assert_eq!(
            state_from_arg("create:mechanical_belt[facing=north]"),
            Some(belt)
        );

        // A creative motor drives an adjacent shaft at its driven RPM —
        // no water, wind, or contraption required.
        let (mx, my, mz) = (9_000_008, 80, 900);
        let (sx, sy, sz) = (9_000_008, 80, 901);
        clear(mx, my, mz);
        clear(sx, sy, sz);
        let motor_st = state_of("create:creative_motor", &[("facing", "north")]);
        let shaft_st = state_of("create:shaft", &[("axis", "z")]);
        put(DimId::Overworld, mx, my, mz, motor_st);
        put(DimId::Overworld, sx, sy, sz, shaft_st);
        live_create_with(|rt| {
            let motor = rt
                .factory
                .kinetic_solution(pos_of((mx, my, mz)))
                .expect("motor registers in the kinetic graph");
            assert!(
                motor.capacity > 0.0,
                "creative motor must provide stress capacity"
            );
            let shaft = rt
                .factory
                .kinetic_solution(pos_of((sx, sy, sz)))
                .expect("shaft joins the motor's network");
            assert_eq!(
                shaft.speed, 256.0,
                "shaft must spin at the motor's driven RPM"
            );
        });
        clear(mx, my, mz);
        clear(sx, sy, sz);
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
        put(
            DimId::Overworld,
            gearbox.0,
            gearbox.1,
            gearbox.2,
            gearbox_st,
        );
        let shaft_y_st = state_of("create:shaft", &[("axis", "y")]);
        put(
            DimId::Overworld,
            shaft_y.0,
            shaft_y.1,
            shaft_y.2,
            shaft_y_st,
        );
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
        let tags = create_adapter::neoforge_wire::decode_kinetic_update_batch(&rest[chan_len..])
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
        let rotated =
            live_create_wrench_rotate(pos_of(shaft), shaft_st).expect("wrench rotates shaft");
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

    /// A 12-byte crafted `MCF2` — magic + VarInt `count = 1_000_000` + one pad
    /// byte + a *valid* FNV trailer — must fail closed on the entry walk
    /// without pre-allocating `count` entries (~44 MB).
    #[test]
    fn create_sidecar_huge_count_fails_without_huge_alloc() {
        let mut body = Vec::new();
        body.extend_from_slice(LIVE_CREATE_SIDECAR_MAGIC);
        write_varint(&mut body, 1_000_000);
        body.push(0);
        let sum = live_create_fnv32(&body);
        body.extend_from_slice(&sum.to_be_bytes());
        assert_eq!(body.len(), 12);
        assert!(live_create_decode_sidecar(&body).is_err());
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
        put(
            DimId::Overworld,
            wx,
            wy,
            wz,
            state_of("create:water_wheel", &[("axis", "x")]),
        );
        put(
            DimId::Overworld,
            shaft.0,
            shaft.1,
            shaft.2,
            state_of("create:shaft", &[("axis", "x")]),
        );
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
        me.inv.lock().unwrap_or_else(|e| e.into_inner()).remove(&36);
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
        me.inv.lock().unwrap_or_else(|e| e.into_inner()).remove(&36);
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
        assert!(live_create_interact_block(
            &me,
            DimId::Overworld,
            bx,
            by,
            bz,
            belt_st,
            0
        ));
        assert_eq!(
            me.inv
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&36)
                .map(|s| s.1),
            Some(1)
        );
        live_create_with(|rt| assert!(rt.duct_machines.contains_key(&belt_pos)));

        // After the traversal delay the item reaches the machine output.
        let machine = live_create_with(|rt| rt.duct_machines[&belt_pos]).unwrap();
        assert!(step_until(200, |rt| rt
            .factory
            .scheduler()
            .region(belt_pos.region_key())
            .is_some_and(|r| r
                .transport()
                .machine(machine)
                .is_some_and(|m| m.output_items == 1))));

        // Empty-handed click collects it back into the inventory.
        me.inv.lock().unwrap_or_else(|e| e.into_inner()).remove(&36);
        assert!(live_create_interact_block(
            &me,
            DimId::Overworld,
            bx,
            by,
            bz,
            belt_st,
            0
        ));
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
        use wire_mods_tests::{lmb_decode_payload, lmb_read_frame, lmb_socket_pair};

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

        // Join sends the dynamic registry before the dimension-scoped snapshot.
        let (mut send, mut recv) = lmb_socket_pair();
        live_create_sync_on_join(&mut send);
        drop(send);
        let mut stash = Vec::new();
        let (id, body) = lmb_read_frame(&mut recv, &mut stash).expect("registry frame");
        assert_eq!(id, LIVE_MOD_S2C_CUSTOM_PAYLOAD_ID);
        let (channel, payload) = lmb_decode_payload(&body);
        assert_eq!(channel, MOD_REGISTRY_SYNC_CHANNEL);
        mod_adapter_core::live_registry_bridge::decode_registry_sync_frame(&payload)
            .expect("registry decodes");

        let (id, body) = lmb_read_frame(&mut recv, &mut stash).expect("kinetic frame");
        assert_eq!(id, LIVE_MOD_S2C_CUSTOM_PAYLOAD_ID);
        let (channel, payload) = lmb_decode_payload(&body);
        assert_eq!(
            channel,
            create_adapter::neoforge_wire::CHANNEL_CREATE_KINETIC_SYNC
        );
        let chunk = create_adapter::neoforge_wire::decode_kinetic_batch_chunk(&payload)
            .expect("snapshot chunk decodes");
        assert_eq!(chunk.dimension_id, DimId::Overworld.raw() as i32);
        assert_eq!(chunk.snapshot_generation, TICK_IDX.load(Ordering::Relaxed));
        assert_eq!((chunk.chunk_index, chunk.chunk_total), (0, 1));
        assert!(chunk.tags.iter().any(|t| t.pos == pos_of(W4)));
        assert!(lmb_read_frame(&mut recv, &mut stash).is_none());
        assert!(stash.is_empty(), "no partial frame after the snapshot");

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
        assert_eq!(reg.get_prop(rotated, "axis").as_deref(), Some("y"));
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

    /// Regression: `live_create_tick_body` must drive the factory through
    /// `CreateFactory::step_tick`'s parallel region stepping and idle-sleep
    /// bookkeeping — not just drain kinetic tags. A placed network creates
    /// a real region; once it goes idle the scheduler parks it and later
    /// ticks skip it entirely (counted in `idle_export_skips`).
    #[test]
    fn create_factory_parallel_step_and_idle_sleep_are_wired() {
        let _g = world_guard();
        arm();
        let (wx, wy, wz) = W8;
        // water -> water_wheel(x) -> shaft(x): one kinetic region, no items.
        let water = (wx + 1, wy, wz);
        let shaft = (wx - 1, wy, wz);
        for c in [water, shaft] {
            clear(c.0, c.1, c.2);
        }
        clear(wx, wy, wz);
        put(DimId::Overworld, water.0, water.1, water.2, TEST_WATER);
        put(
            DimId::Overworld,
            wx,
            wy,
            wz,
            state_of("create:water_wheel", &[("axis", "x")]),
        );
        put(
            DimId::Overworld,
            shaft.0,
            shaft.1,
            shaft.2,
            state_of("create:shaft", &[("axis", "x")]),
        );

        // First live tick: the region is stepped through step_tick.
        let _ = live_create_tick_body();
        live_create_with(|rt| {
            assert!(
                rt.factory.active_regions_last_tick() >= 1 || rt.factory.sleeping_regions() >= 1,
                "step_tick must step or park the placed region"
            );
        });

        // Idle region parks: subsequent ticks skip it entirely and count
        // the skipped exports — proof the sleep path is live, not
        // decorative. (`active_regions_last_tick` can transiently report
        // scheduler error results if a concurrent test rewinds the shared
        // tick clock, so the counters are the stable signal.)
        for _ in 0..4 {
            let _ = live_create_tick_body();
        }
        live_create_with(|rt| {
            assert!(
                rt.factory.sleeping_regions() >= 1,
                "idle region must be parked asleep"
            );
            assert!(
                rt.factory.idle_export_skips() > 0,
                "skipped idle exports must be counted"
            );
        });

        for c in [water, shaft] {
            clear(c.0, c.1, c.2);
        }
        clear(wx, wy, wz);
    }

    // ---------- .nbt schematic placement + end-to-end duct flow ----------

    fn nbt_name(out: &mut Vec<u8>, name: &str) {
        out.extend_from_slice(&(name.len() as u16).to_be_bytes());
        out.extend_from_slice(name.as_bytes());
    }

    fn nbt_str(name: &str, value: &str) -> Vec<u8> {
        let mut t = vec![8];
        nbt_name(&mut t, name);
        t.extend_from_slice(&(value.len() as u16).to_be_bytes());
        t.extend_from_slice(value.as_bytes());
        t
    }

    fn nbt_int(name: &str, value: i32) -> Vec<u8> {
        let mut t = vec![3];
        nbt_name(&mut t, name);
        t.extend_from_slice(&value.to_be_bytes());
        t
    }

    /// `name: [x, y, z]` as a `TAG_List` of three `TAG_Int` — the spelling
    /// Structure Blocks write for `size`/`pos`.
    fn nbt_int3(name: &str, v: [i32; 3]) -> Vec<u8> {
        let mut t = vec![9];
        nbt_name(&mut t, name);
        t.push(3);
        t.extend_from_slice(&3i32.to_be_bytes());
        for n in v {
            t.extend_from_slice(&n.to_be_bytes());
        }
        t
    }

    /// `TAG_Compound` payload (named children + `TAG_End`) without a
    /// tag/name header — the shape list elements and the root take.
    fn nbt_compound_payload(fields: Vec<Vec<u8>>) -> Vec<u8> {
        let mut p = Vec::new();
        for f in fields {
            p.extend_from_slice(&f);
        }
        p.push(0);
        p
    }

    fn nbt_named_compound(name: &str, fields: Vec<Vec<u8>>) -> Vec<u8> {
        let mut t = vec![10];
        nbt_name(&mut t, name);
        t.extend_from_slice(&nbt_compound_payload(fields));
        t
    }

    fn nbt_list(name: &str, elem_tag: u8, items: Vec<Vec<u8>>) -> Vec<u8> {
        let mut t = vec![9];
        nbt_name(&mut t, name);
        t.push(elem_tag);
        t.extend_from_slice(&(items.len() as i32).to_be_bytes());
        for i in items {
            t.extend_from_slice(&i);
        }
        t
    }

    /// One structure-block `.nbt` document: `size`, a `palette` of
    /// `(Name, partial Properties)` entries, and `blocks` of
    /// `(pos, state-index)` pairs.
    fn structure_nbt(
        size: [i32; 3],
        palette: &[(&str, &[(&str, &str)])],
        blocks: &[([i32; 3], usize)],
    ) -> Vec<u8> {
        let mut root = nbt_int3("size", size);
        let pal: Vec<Vec<u8>> = palette
            .iter()
            .map(|(id, props)| {
                let mut fields = vec![nbt_str("Name", id)];
                if !props.is_empty() {
                    fields.push(nbt_named_compound(
                        "Properties",
                        props.iter().map(|(k, v)| nbt_str(k, v)).collect(),
                    ));
                }
                nbt_compound_payload(fields)
            })
            .collect();
        root.extend_from_slice(&nbt_list("palette", 10, pal));
        let bl: Vec<Vec<u8>> = blocks
            .iter()
            .map(|(pos, state)| {
                nbt_compound_payload(vec![nbt_int3("pos", *pos), nbt_int("state", *state as i32)])
            })
            .collect();
        root.extend_from_slice(&nbt_list("blocks", 10, bl));
        let mut out = vec![10, 0, 0];
        out.extend_from_slice(&root);
        out.push(0);
        out
    }

    /// Single-member RFC 1952 gzip wrapper around stored deflate blocks —
    /// real framing so the loader exercises its full header/inflate/
    /// CRC32-trailer path.
    fn gzip_stored(data: &[u8]) -> Vec<u8> {
        let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff];
        let mut chunks = data.chunks(0xffff).peekable();
        if chunks.peek().is_none() {
            out.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
        }
        while let Some(chunk) = chunks.next() {
            let len = chunk.len() as u16;
            out.push(u8::from(chunks.peek().is_none()));
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&(!len).to_le_bytes());
            out.extend_from_slice(chunk);
        }
        out.extend_from_slice(&megu_core::hash::crc32_ieee(data).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out
    }

    #[test]
    fn m3c_schematic_preflight_aborts_before_any_cell_write() {
        let _g = world_guard();
        arm();
        let (ox, oy, oz) = (9_000_000, 80, 1_300);
        clear(ox, oy, oz);
        clear(ox + 1, oy, oz);
        let stone = name_to_state("stone").expect("stone state");
        put(DimId::Overworld, ox + 1, oy, oz, stone);
        let nbt = structure_nbt(
            [2, 1, 1],
            &[("minecraft:stone", &[])],
            &[([0, 0, 0], 0), ([1, 0, 0], 0)],
        );
        let err = live_place_nbt_schematic_in_dim(DimId::Overworld, (ox, oy, oz), &nbt)
            .expect_err("obstructed schematic must reject in preflight");
        assert!(err.contains("obstructed"), "{err}");
        assert_eq!(block_at_dim(DimId::Overworld, ox, oy, oz), 0);
        assert_eq!(block_at_dim(DimId::Overworld, ox + 1, oy, oz), stone);
        clear(ox + 1, oy, oz);
        LIVE_CREATE_TEST_GATE.store(0, Ordering::Relaxed);
    }

    const W9: (i32, i32, i32) = (9_000_000, 80, 1_100);
    const W10: (i32, i32, i32) = (9_000_000, 80, 1_200);

    /// End-to-end: a gzipped `.nbt` Create factory — creative motor ->
    /// shaft -> mechanical press over a depot fed by chute -> andesite
    /// funnel -> two belts — is placed through
    /// `live_place_nbt_schematic_in_dim`, then an iron ingot dropped into
    /// the chute rides the whole duct chain and lands on the depot as a
    /// pressed `create:iron_sheet`.
    #[test]
    fn create_nbt_schematic_duct_chain_presses_iron() {
        let _g = world_guard();
        arm();
        let (ox, oy, oz) = W9;
        let cells: [[i32; 3]; 10] = [
            [0, 1, 0], // chute
            [0, 0, 0], // funnel (insert mode: staged items push out east)
            [1, 0, 0], // belt
            [2, 0, 0], // belt
            [3, 0, 0], // depot
            [4, 0, 0], // brass casing (decorative)
            [4, 1, 0], // andesite casing (decorative)
            [1, 2, 0], // creative motor
            [2, 2, 0], // shaft
            [3, 2, 0], // mechanical press (2 over the depot)
        ];
        for [dx, dy, dz] in cells {
            clear(ox + dx, oy + dy, oz + dz);
        }
        let nbt = structure_nbt(
            [5, 3, 1],
            &[
                ("create:chute", &[("facing", "east")]),
                (
                    "create:andesite_funnel",
                    &[("extracting", "false"), ("facing", "east")],
                ),
                ("create:belt", &[("facing", "east")]),
                ("create:depot", &[]),
                ("create:brass_casing", &[]),
                ("create:andesite_casing", &[]),
                ("create:creative_motor", &[("facing", "east")]),
                ("create:shaft", &[("axis", "x")]),
                ("create:mechanical_press", &[("facing", "east")]),
            ],
            &[
                (cells[0], 0),
                (cells[1], 1),
                (cells[2], 2),
                (cells[3], 2),
                (cells[4], 3),
                (cells[5], 4),
                (cells[6], 5),
                (cells[7], 6),
                (cells[8], 7),
                (cells[9], 8),
            ],
        );
        let report =
            live_place_nbt_schematic_in_dim(DimId::Overworld, (ox, oy, oz), &gzip_stored(&nbt))
                .expect("schematic must place");
        assert_eq!(report.placed_blocks, 10);
        assert_eq!(report.unresolved_palette_entries, 0);
        assert_eq!(report.skipped_blocks, 0);
        assert_eq!(
            report.kinetic_nodes_armed, 10,
            "every Create cell must register in the live runtime"
        );

        // The placed press must be a Running node on the motor's network.
        let press_pos = pos_of((ox + 3, oy + 2, oz));
        live_create_with(|rt| {
            let sol = rt
                .factory
                .kinetic_solution(press_pos)
                .expect("press registers in the kinetic graph");
            assert!(
                matches!(
                    sol.status,
                    mod_offload::kinetic_graph::NetworkStatus::Running
                ),
                "press must be powered, got {:?}",
                sol.status
            );
        });

        // Drop one ingot into the chute and let the duct chain carry it.
        let iron = name_to_item("minecraft:iron_ingot").expect("iron item");
        assert!(live_create_duct_insert(
            DimId::Overworld,
            ox,
            oy + 1,
            oz,
            iron
        ));
        let depot_pos = pos_of((ox + 3, oy, oz));
        let sheet = name_to_item("create:iron_sheet").expect("sheet item");
        assert!(
            step_until(2_000, |rt| rt
                .held
                .get(&depot_pos)
                .is_some_and(|h| h.items.as_slice() == [sheet])),
            "iron must flow chute -> funnel -> belts -> depot and press to a sheet"
        );

        for [dx, dy, dz] in cells {
            clear(ox + dx, oy + dy, oz + dz);
        }
    }

    /// The extracting pull path: a belt dead-ending into air accumulates
    /// items in its output FIFO; a `create:brass_funnel` facing the belt
    /// with `extracting=true` pulls one per cycle into the depot attached
    /// behind it.
    #[test]
    fn create_extracting_funnel_pulls_belt_end_into_depot() {
        let _g = world_guard();
        arm();
        let (ox, oy, oz) = W10;
        let belt = (ox, oy, oz);
        let funnel = (ox + 1, oy, oz);
        let depot = (ox + 2, oy, oz);
        for c in [belt, funnel, depot] {
            clear(c.0, c.1, c.2);
        }
        put(
            DimId::Overworld,
            belt.0,
            belt.1,
            belt.2,
            state_of("create:belt", &[("facing", "north")]),
        );
        put(
            DimId::Overworld,
            funnel.0,
            funnel.1,
            funnel.2,
            state_of(
                "create:brass_funnel",
                &[("facing", "west"), ("extracting", "true")],
            ),
        );
        put(
            DimId::Overworld,
            depot.0,
            depot.1,
            depot.2,
            state_of("create:depot", &[]),
        );

        let iron = name_to_item("minecraft:iron_ingot").expect("iron item");
        assert!(live_create_duct_insert(
            DimId::Overworld,
            belt.0,
            belt.1,
            belt.2,
            iron
        ));
        let depot_pos = pos_of(depot);
        assert!(
            step_until(200, |rt| rt
                .held
                .get(&depot_pos)
                .is_some_and(|h| h.items.as_slice() == [iron])),
            "extracting funnel must pull the belt-end item into the depot"
        );

        for c in [belt, funnel, depot] {
            clear(c.0, c.1, c.2);
        }
    }

    // ---------- SERVER-LIVE-MOD-BRIDGE-OPT-WIRE ----------

    /// Fresh coordinates for the optimizer/forge tests, disjoint from the
    /// `W1`..`W10` ranges the transport tests own.
    const WOPT1: (i32, i32, i32) = (9_000_008, 80, 1_400);
    const WOPT2: (i32, i32, i32) = (9_000_008, 80, 1_500);
    const WOPT3: (i32, i32, i32) = (9_000_008, 80, 1_600);

    /// Reset every `live_opt_wire` process-global between tests so stale
    /// cadences, memo entries, forge endpoints and catch-up limits never
    /// leak across cases.
    fn opt_reset() {
        live_auto_optimizer_reset();
        *live_lazy_catchup_state()
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = LiveLazyCatchUpState::default();
        *live_colossal_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = LiveColossalAndForgeRegistry::default();
        live_cap_reset();
        // The block probe's id source prefers the JAR registry plan —
        // drop any plan a previous test installed so an empty world cell
        // probes as air/unloaded rather than a stale modded id.
        {
            let mut suite = live_mod_suite().lock().unwrap_or_else(|e| e.into_inner());
            suite.registry_plan = None;
        }
    }

    #[test]
    fn live_mod_opt_memoizer_hits_misses_and_invalidates() {
        let _g = world_guard();
        opt_reset();
        let calls = std::sync::atomic::AtomicU64::new(0);
        let fp = live_fingerprint_held_items(&[5, 7]);

        // Miss: compute runs exactly once, its result is returned...
        let first = live_auto_recipe_lookup(LIVE_OPT_MACHINE_PRESS, fp, |_| {
            calls.fetch_add(1, Ordering::Relaxed);
            Some(3)
        });
        assert_eq!(first, Some(3));
        // ...and the identical fingerprint hits the cache without
        // re-running compute — even a different result never lands.
        let second = live_auto_recipe_lookup(LIVE_OPT_MACHINE_PRESS, fp, |_| {
            calls.fetch_add(1, Ordering::Relaxed);
            Some(9)
        });
        assert_eq!(second, Some(3));
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        // Negative results cache on equal footing: a `None` compute runs
        // once and then answers every subsequent lookup for free.
        let miss_fp = fp.wrapping_add(1);
        let third = live_auto_recipe_lookup(LIVE_OPT_MACHINE_PRESS, miss_fp, |_| {
            calls.fetch_add(1, Ordering::Relaxed);
            None
        });
        let fourth = live_auto_recipe_lookup(LIVE_OPT_MACHINE_PRESS, miss_fp, |_| {
            calls.fetch_add(1, Ordering::Relaxed);
            Some(1)
        });
        assert_eq!(third, None);
        assert_eq!(fourth, None);
        assert_eq!(calls.load(Ordering::Relaxed), 2);

        // Kind is part of the key: the same fingerprint under the mixer
        // table misses again instead of cross-hitting the press verdict.
        let fifth = live_auto_recipe_lookup(LIVE_OPT_MACHINE_MIXER, fp, |_| {
            calls.fetch_add(1, Ordering::Relaxed);
            Some(2)
        });
        assert_eq!(fifth, Some(2));
        assert_eq!(calls.load(Ordering::Relaxed), 3);

        // A registry revision bump (the `scan_mods_dir` invalidation
        // channel) clears every cached verdict — the next lookup
        // re-resolves and runs compute again.
        {
            let mut suite = live_mod_suite().lock().unwrap_or_else(|e| e.into_inner());
            suite.registry_revision = suite.registry_revision.wrapping_add(1);
        }
        let sixth = live_auto_recipe_lookup(LIVE_OPT_MACHINE_PRESS, fp, |_| {
            calls.fetch_add(1, Ordering::Relaxed);
            Some(7)
        });
        assert_eq!(sixth, Some(7));
        assert_eq!(calls.load(Ordering::Relaxed), 4);

        let stats = live_mod_auto_optimizer_stats();
        assert_eq!(stats.memo_misses, 4);
        assert_eq!(stats.memo_hits, 1);
        assert_eq!(stats.memo_negative_hits, 1);
        // The invalidation cleared the table; only the post-bump
        // recompute stands.
        assert_eq!(stats.memo_len, 1);
    }

    #[test]
    fn live_mod_opt_backoff_escalates_and_wakes() {
        let _g = world_guard();
        opt_reset();
        let pos = pos_of(WOPT2);

        // Fresh machine: first poll grants and schedules cadence 1.
        assert!(live_auto_machine_should_tick(pos, 100));
        // Four unchanged polls -> ladder step 2 (2-tick interval).
        for _ in 0..4 {
            live_auto_machine_record(pos, false);
        }
        assert!(!live_auto_machine_should_tick(pos, 101));
        assert!(live_auto_machine_should_tick(pos, 102));
        // Four more unchanged -> step 4: ticks 103..106 all skip.
        for _ in 0..4 {
            live_auto_machine_record(pos, false);
        }
        assert!(!live_auto_machine_should_tick(pos, 105));
        assert!(live_auto_machine_should_tick(pos, 106));
        // A dirty-event wake restores cadence 1 with 0-tick latency: the
        // very same tick grants another poll instead of waiting out the
        // 4-tick step.
        live_auto_machine_wake_positions(&[pos]);
        assert!(live_auto_machine_should_tick(pos, 106));
        // A changed result also snaps back to cadence 1.
        live_auto_machine_record(pos, true);
        assert!(live_auto_machine_should_tick(pos, 107));

        let stats = live_mod_auto_optimizer_stats();
        assert_eq!(stats.backoff_tracked, 1);
        assert!(stats.backoff_ticks_saved >= 2);
    }

    /// The live depot+press path: identical held contents hit the
    /// memoizer every poll, an unmatched product backs the machine off,
    /// and a hopper pull / scripted feed wakes it at 0-tick latency.
    #[test]
    fn live_mod_opt_depot_press_memoizes_and_backs_off() {
        let _g = world_guard();
        arm();
        opt_reset();
        let (wx, wy, wz) = WOPT1;
        // Same powered press-over-depot rig as the right-click test.
        let water = (wx + 1, wy, wz);
        let shaft = (wx - 1, wy, wz);
        let press = (wx - 2, wy, wz);
        let depot = (wx - 2, wy - 2, wz);
        for c in [water, shaft, press, depot] {
            clear(c.0, c.1, c.2);
        }
        put(DimId::Overworld, water.0, water.1, water.2, TEST_WATER);
        put(
            DimId::Overworld,
            wx,
            wy,
            wz,
            state_of("create:water_wheel", &[("axis", "x")]),
        );
        put(
            DimId::Overworld,
            shaft.0,
            shaft.1,
            shaft.2,
            state_of("create:shaft", &[("axis", "x")]),
        );
        put(
            DimId::Overworld,
            press.0,
            press.1,
            press.2,
            state_of("create:mechanical_press", &[("facing", "east")]),
        );
        put(
            DimId::Overworld,
            depot.0,
            depot.1,
            depot.2,
            state_of("create:depot", &[]),
        );
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

        // Feed the depot through the shared duct-insert entry point and
        // press the ingot to a sheet. Every granted poll after the first
        // reads the memoized recipe match — the matcher never rescans.
        let iron = name_to_item("minecraft:iron_ingot").expect("iron item");
        assert!(live_create_duct_insert(
            DimId::Overworld,
            depot.0,
            depot.1,
            depot.2,
            iron
        ));
        let sheet = name_to_item("create:iron_sheet").expect("sheet item");
        assert!(
            step_until(2_000, |rt| rt
                .held
                .get(&depot_pos)
                .is_some_and(|h| h.items.as_slice() == [sheet])),
            "iron must press to a sheet"
        );
        let stats = live_mod_auto_optimizer_stats();
        assert_eq!(stats.memo_misses, 1, "the first lookup misses once");
        assert!(
            stats.memo_hits >= 1,
            "identical held contents must hit the memoizer"
        );

        // The finished sheet matches no recipe: the machine goes idle,
        // its verdict negative-caches, and its cadence slides down the
        // backoff ladder so most ticks cost one probe.
        for _ in 0..40 {
            TICK_IDX.fetch_add(1, Ordering::Relaxed);
            let _ = live_create_tick_body();
        }
        let stats = live_mod_auto_optimizer_stats();
        assert!(
            stats.memo_negative_hits >= 1,
            "unmatched contents must negative-cache, got {stats:?}"
        );
        assert!(
            stats.backoff_ticks_saved > 0,
            "an idle machine must save polls, got {stats:?}"
        );

        // Hopper-style pull of the product fires the dirty-event wake —
        // the next poll grants immediately instead of sleeping out the
        // ladder step.
        let pulled =
            live_cap_extract_item(DimId::Overworld, depot.0, depot.1, depot.2, None, 1, false);
        assert_eq!(
            pulled.map(|s| (s.item_id, s.count)),
            Some(("create:iron_sheet".to_string(), 1))
        );
        let next = TICK_IDX.load(Ordering::Relaxed) + 1;
        assert!(
            live_auto_machine_should_tick(depot_pos, next),
            "a pull wake must grant the very next poll"
        );

        // A fresh feed wakes + presses again end to end.
        assert!(live_create_duct_insert(
            DimId::Overworld,
            depot.0,
            depot.1,
            depot.2,
            iron
        ));
        assert!(
            step_until(2_000, |rt| rt
                .held
                .get(&depot_pos)
                .is_some_and(|h| h.items.as_slice() == [sheet])),
            "re-fed iron must press to a sheet after the wake"
        );

        for c in [water, shaft, press, depot] {
            clear(c.0, c.1, c.2);
        }
    }

    /// Delta-coalescer and bulk-batcher surface: duplicate payloads drop,
    /// mid-tier churn throttles, and `A -> B -> A` section writes cancel
    /// before the drain.
    #[test]
    fn live_mod_opt_delta_coalescer_and_bulk_batcher() {
        let _g = world_guard();
        opt_reset();

        // First payload ships; the byte-identical repeat drops; a changed
        // payload inside the Mid window defers; after the interval it
        // ships.
        assert!(live_mod_delta_should_emit(7, b"payload-a", 100, 0));
        assert!(!live_mod_delta_should_emit(7, b"payload-a", 101, 0));
        assert!(!live_mod_delta_should_emit(7, b"payload-b", 102, 1));
        assert!(live_mod_delta_should_emit(7, b"payload-b", 104, 1));
        // Out-of-range sources never ship.
        assert!(!live_mod_delta_should_emit(8, b"payload-c", 104, 9));

        // `A -> B -> A` cancels outright; the lone surviving unlit write
        // keeps the batch's `dirty_light` clean.
        live_mod_bulk_record_set_block(DimId::Overworld, 5, 64, 5, 0, 100);
        live_mod_bulk_record_set_block(DimId::Overworld, 5, 64, 5, 100, 0);
        live_mod_bulk_record_set_block_unlit(DimId::Overworld, 6, 64, 5, 0, 200);
        let batches = live_mod_bulk_drain();
        assert_eq!(batches.len(), 1, "cancelled writes must not drain");
        assert_eq!(batches[0].cells.len(), 1);
        assert_eq!(batches[0].cells[0].new_state, 200);
        // `dirty_light` is conservative: the cancelled lit write still
        // taints the section even though no lit change survives.
        assert!(batches[0].dirty_light);
        let stats = live_mod_auto_optimizer_stats();
        assert_eq!(stats.bulk_raw_writes, 3);
        assert_eq!(stats.bulk_coalesced_writes, 1);
        assert_eq!(stats.bulk_cancelled_writes, 1);
        assert!(live_mod_bulk_drain().is_empty());
    }

    /// Fail-closed catch-up: the default state does zero work, the
    /// explicit arm computes the exact O(1) result, and headless nodes
    /// accumulate output across ticks with carry.
    #[test]
    fn live_mod_lazy_catchup_disabled_default_exact_when_enabled() {
        let _g = world_guard();
        opt_reset();

        // Fail-closed by default: the report echoes the state and
        // reports zero work.
        assert!(!live_mod_lazy_catchup_enabled());
        let furnace = FurnaceCatchUpState {
            input_count: 10,
            fuel_count: 10,
            fuel_burn_ticks_each: 200,
            current_burn_remaining: 0,
            cook_progress: 0,
            cook_time_total: 200,
            output_count: 0,
            output_max_stack: 64,
        };
        let report = live_mod_lazy_catchup_furnace(&furnace, 400);
        assert_eq!(report.items_smelted, 0);
        assert_eq!(report.fuel_consumed, 0);
        assert_eq!(report.state, furnace);

        // Armed explicitly: 400 ticks at 200 ticks/smelt smelts exactly
        // two items and consumes exactly two fuel units.
        live_mod_lazy_catchup_set_enabled(true);
        assert!(live_mod_lazy_catchup_enabled());
        let report = live_mod_lazy_catchup_furnace(&furnace, 400);
        assert_eq!(report.items_smelted, 2);
        assert_eq!(report.fuel_consumed, 2);
        assert_eq!(report.state.input_count, 8);
        assert_eq!(report.state.output_count, 2);
        assert_eq!(report.state.fuel_count, 8);
        assert_eq!(report.state.cook_progress, 0);
        assert_eq!(report.state.current_burn_remaining, 0);

        // Growth: a guaranteed random tick advances deterministically to
        // the stage cap; a seeded draw is monotone and bounded.
        assert_eq!(live_mod_lazy_catchup_growth(2, 7, 1.0, 10, 42), 7);
        let seeded = live_mod_lazy_catchup_growth(0, 7, 0.5, 20, 1234);
        assert!(seeded <= 7);
        let seeded_more = live_mod_lazy_catchup_growth(0, 7, 0.5, 40, 1234);
        assert!(seeded_more >= seeded);

        // Machine batch: 50 ticks at 5 ticks/cycle = 10 slots, input of 5
        // caps the cycles; 10 energy/cycle drains 50 of the 100 buffer.
        let machine = MachineCatchUpState {
            input_count: 5,
            energy: 100,
            energy_per_cycle: 10,
            ticks_per_cycle: 5,
            cycle_progress: 0,
            output_count: 0,
            output_space: 64,
        };
        let mreport = live_mod_lazy_catchup_machine(&machine, 50);
        assert_eq!(mreport.cycles_completed, 5);
        assert_eq!(mreport.energy_consumed, 50);
        assert_eq!(mreport.state.input_count, 0);
        assert_eq!(mreport.state.output_count, 5);
        assert_eq!(mreport.state.energy, 50);

        // Headless factory: a 0.5 item/tick node carries its half-item
        // remainder across steps — two ticks move one item, the banked
        // carry plus one more tick moves the second.
        let node = HeadlessFactoryNode {
            node_id: 7,
            dim_id: 0,
            cx: 0,
            cz: 0,
            rate_per_tick_milli: 500,
            input_buffer: 10,
            output_buffer: 0,
            max_buffer: 64,
            carry_milli: 0,
        };
        assert!(live_mod_headless_factory_register(node));
        // Duplicate ids refuse.
        assert!(!live_mod_headless_factory_register(node));
        assert_eq!(live_mod_headless_factory_len(), 1);
        assert_eq!(live_mod_headless_factory_step(2), 1);
        assert_eq!(live_mod_headless_factory_step(1), 0);
        assert_eq!(live_mod_headless_factory_step(1), 1);
        assert!(live_mod_headless_factory_remove(7));
        assert_eq!(live_mod_headless_factory_len(), 0);

        // Disabled again mid-flight: every entry point goes back to
        // echoing state and moving nothing.
        live_mod_lazy_catchup_set_enabled(false);
        let report = live_mod_lazy_catchup_furnace(&furnace, 400);
        assert_eq!(report.items_smelted, 0);
        assert_eq!(report.state, furnace);
    }

    /// The multi-block forge on the live capability bus: hopper pushes
    /// feed the input buffer, breath + ticks smelt, and hopper pulls
    /// collect the product — all through the same
    /// `live_cap_insert_item`/`live_cap_extract_item` paths vanilla
    /// hoppers use.
    #[test]
    fn live_mod_forge_bus_round_trip_and_multipart_hitbox() {
        use mod_adapter_core::colossal_and_forge::{
            forge_shell_3x3, AlloyRecipe, ForgeCellExpect, MultiPartHitbox, PartKind,
            PartOffsetSpec,
        };
        let _g = world_guard();
        arm();
        opt_reset();
        let (fx, fy, fz) = WOPT3;

        // An all-Air shell validates in the empty test world regardless
        // of whether `breg()` is armed — `Air` accepts an unloaded probe
        // or any `minecraft:*_air` id.
        let spec = MultiBlockForgeSpec {
            cells: forge_shell_3x3(ForgeCellExpect::Air, ForgeCellExpect::Air, [0, 0, 1]),
            heat_elements: vec![ElementKind::Fire],
            heat_capacity: 100.0,
            heat_per_breath_tick: 5.0,
            idle_heat_decay_per_tick: 0.0,
            recipes: vec![AlloyRecipe {
                base_ingot: "anyforge:iron".to_string(),
                catalyst_item: "anyforge:catalyst".to_string(),
                element: ElementKind::Fire,
                heat_cost: 50.0,
                smelt_ticks: 2,
                output_item: "anyforge:alloy".to_string(),
                output_count: 1,
            }],
        };
        assert!(live_forge_register(
            DimId::Overworld,
            fx,
            fy,
            fz,
            spec.clone()
        ));

        // Hopper push: recipe inputs land in the forge buffer; a
        // non-recipe item refuses outright (all-or-nothing).
        assert_eq!(
            live_cap_insert_item(
                DimId::Overworld,
                fx,
                fy,
                fz,
                None,
                "anyforge:iron",
                1,
                &[],
                false
            ),
            1
        );
        assert_eq!(
            live_cap_insert_item(
                DimId::Overworld,
                fx,
                fy,
                fz,
                None,
                "anyforge:catalyst",
                1,
                &[],
                false
            ),
            1
        );
        assert_eq!(
            live_cap_insert_item(
                DimId::Overworld,
                fx,
                fy,
                fz,
                None,
                "anyforge:junk",
                1,
                &[],
                false
            ),
            0
        );
        // The output buffer is still empty — nothing pulls yet.
        assert!(live_cap_extract_item(DimId::Overworld, fx, fy, fz, None, 64, false).is_none());

        // 20 breath ticks * 5.0 = 100 heat >= the 50.0 recipe cost;
        // tick 1 starts the smelt, tick 3 completes it (smelt_ticks=2).
        assert_eq!(
            live_forge_apply_breath(DimId::Overworld, fx, fy, fz, ElementKind::Fire, 20),
            Some(true)
        );
        // Ice is not in `heat_elements` — refused.
        assert_eq!(
            live_forge_apply_breath(DimId::Overworld, fx, fy, fz, ElementKind::Ice, 20),
            Some(false)
        );
        let ev1 = live_forge_tick(DimId::Overworld, fx, fy, fz).expect("registered forge");
        assert_eq!(ev1.smelt_started, Some(0));
        let ev2 = live_forge_tick(DimId::Overworld, fx, fy, fz).expect("registered forge");
        assert!(ev2.smelt_completed.is_none());
        let ev3 = live_forge_tick(DimId::Overworld, fx, fy, fz).expect("registered forge");
        assert_eq!(ev3.smelt_completed, Some(0));

        // Hopper pull: the smelted product comes out through the same
        // extract path a vanilla hopper calls.
        let out = live_cap_extract_item(DimId::Overworld, fx, fy, fz, None, 64, false)
            .expect("smelted output must pull");
        assert_eq!(out.item_id, "anyforge:alloy");
        assert_eq!(out.count, 1);
        assert!(live_cap_extract_item(DimId::Overworld, fx, fy, fz, None, 64, false).is_none());

        // Multi-part hitbox: a head weak point outranks the armored torso
        // when the ray crosses both, and a side ray sees only the torso.
        live_colossal_register_hitbox(
            "anyforge:wyrm",
            MultiPartHitbox {
                parts: vec![
                    PartOffsetSpec {
                        kind: PartKind::Head,
                        local_offset: [0.0, 2.0, 0.0],
                        half_extents: [0.5, 0.5, 0.5],
                        damage_multiplier: None,
                    },
                    PartOffsetSpec {
                        kind: PartKind::Torso,
                        local_offset: [0.0, 0.0, 0.0],
                        half_extents: [1.0, 1.0, 1.0],
                        damage_multiplier: None,
                    },
                ],
            },
        );
        let head = live_colossal_hit_test(
            "anyforge:wyrm",
            [0.0, 0.0, 0.0],
            0.0,
            0.0,
            1.0,
            [0.0, 4.0, 0.0],
            [0.0, -1.0, 0.0],
            32.0,
        )
        .expect("downward ray must strike the head first");
        assert_eq!(head.kind, PartKind::Head);
        assert!((head.damage_multiplier - 1.25).abs() < f32::EPSILON);
        let torso = live_colossal_hit_test(
            "anyforge:wyrm",
            [0.0, 0.0, 0.0],
            0.0,
            0.0,
            1.0,
            [3.0, 0.5, 0.0],
            [-1.0, 0.0, 0.0],
            32.0,
        )
        .expect("side ray must strike the torso");
        assert_eq!(torso.kind, PartKind::Torso);
        assert!((torso.damage_multiplier - 0.9).abs() < f32::EPSILON);
        // A clean miss and an unknown id both fail closed.
        assert!(live_colossal_hit_test(
            "anyforge:wyrm",
            [0.0, 0.0, 0.0],
            0.0,
            0.0,
            1.0,
            [9.0, 9.0, 9.0],
            [1.0, 0.0, 0.0],
            1.0,
        )
        .is_none());
        assert!(live_colossal_hit_test(
            "anyforge:ghost",
            [0.0, 0.0, 0.0],
            0.0,
            0.0,
            1.0,
            [0.0, 0.0, 0.0],
            [0.0, -1.0, 0.0],
            32.0,
        )
        .is_none());

        assert!(live_forge_unregister(DimId::Overworld, fx, fy, fz));
        assert!(live_forge_tick(DimId::Overworld, fx, fy, fz).is_none());
    }

    /// A registered breath profile evaluates a real cone against live
    /// entities: targets inside the aperture take hits sorted by
    /// distance, targets outside or behind the mouth do not.
    #[test]
    fn live_mod_breath_cone_hits_entities_in_aperture() {
        let _g = world_guard();
        opt_reset();
        live_breath_register("anyforge:wyrm", ElementalBreathSpec::fire());
        let entities = vec![
            BreathEntity {
                entity_id: 11,
                center: [0.5, 0.5, 10.0],
                half_extents: [0.5, 0.5, 0.5],
                conductive: false,
            },
            BreathEntity {
                entity_id: 22,
                center: [0.0, 0.0, 30.0],
                half_extents: [0.5, 0.5, 0.5],
                conductive: false,
            },
            BreathEntity {
                entity_id: 33,
                center: [0.0, 0.0, -10.0],
                half_extents: [0.5, 0.5, 0.5],
                conductive: false,
            },
        ];
        // Yaw 0 faces +Z: id 11 sits inside the cone, id 22 outranges the
        // 16-block reach, id 33 stands behind the mouth.
        let outcome = live_breath_evaluate(
            "anyforge:wyrm",
            DimId::Overworld,
            [0.0, 0.0, 0.0],
            0.0,
            0.0,
            &entities,
        )
        .expect("registered breath");
        assert_eq!(
            outcome
                .entity_hits
                .iter()
                .map(|h| h.entity_id)
                .collect::<Vec<_>>(),
            vec![11],
            "only the in-cone entity hits, got {:?}",
            outcome.entity_hits
        );
        assert!((outcome.entity_hits[0].damage - 2.0).abs() < f32::EPSILON);
        // Unknown ids fail closed.
        assert!(live_breath_evaluate(
            "anyforge:ghost",
            DimId::Overworld,
            [0.0, 0.0, 0.0],
            0.0,
            0.0,
            &entities,
        )
        .is_none());
    }
}
