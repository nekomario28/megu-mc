// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Built-in zero-JVM automatic mod adapter engine.
//!
//! One entry point — [`auto_adapt_jar`] for a single JAR or
//! [`auto_adapt_mods_dir`] for a whole `mods/` folder — runs the complete
//! adaptation pipeline over user-supplied mod JARs with no JVM and no
//! operator configuration:
//!
//! 1. **Static security and capability routing.** Every JAR's class files
//!    are inspected without a JVM before superseded or engine-replacement
//!    mods can be skipped. Dangerous archives are quarantined; clean native
//!    engine replacements are skipped with the routing evidence recorded.
//! 2. **Loader metadata.** [`crate::ecosystem_compat::scan_mod_jar_named`] reads
//!    `fabric.mod.json`, `quilt.mod.json`, and `META-INF/neoforge.mods.toml`
//!    / `META-INF/mods.toml` through the crate's fail-closed ZIP reader,
//!    producing the loader kind, declared mod ids, version, environment,
//!    declared Minecraft range, mixin configs, and dependency table plus
//!    the loader-extension report (Jar-in-Jar, access transformers).
//! 3. **Generic data discovery.** `data/<ns>/recipes`, `tags`, `dimension`,
//!    `dimension_type`, `loot_table`, `worldgen`, and culinary members are
//!    counted and fed to [`CulinaryRegistry`] and
//!    [`discover_dimension_mod_entries`]; an embedded
//!    [`EMBEDDED_BUNDLE_MEMBER`] member is validated as a native
//!    [`DeclarativeModBundle`].
//! 4. **Mixin bytecode.** [`scan_jar_mixins_with_options`] parses `.class`
//!    bytecode directly, and the lifter binds every `@Inject` site to a
//!    standard event hook or reports it unliftable with its reason.
//! 5. **Tier synthesis.** A validated `mc-spine-mod-bundle/1` document is
//!    produced for native JARs (an embedded bundle preferred, otherwise one
//!    synthesized from the discovered data) and the JAR lands in
//!    [`AdaptationTier`]: `AutoSkipped`, `NativeBundle`,
//!    `Tier2LiftedShim`, or `Tier3FallbackRequired` carrying exact
//!    human-readable blockers.
//!
//! [`scaffold_native_bundle`] emits the same `mc-spine-mod-bundle/1`
//! document as a starter for mod authors building native zero-JVM Megu
//! mods, and [`write_adaptation_outputs`] persists the per-mod
//! `<mod_id>.bundle.json` files plus the deterministic
//! [`ADAPTATION_REPORT_FILE`] report.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use mc_spine_core::id::ResourceLocation;

use crate::class_mixin_scanner::{
    scan_jar_mixins_with_options, JarMixinScanReport, JarScanOptions,
};
use crate::culinary_adapter::{CulinaryDiscoveryStats, CulinaryRegistry};
use crate::data_loader::{
    parse_json, parse_mod_recipe_json, DataLoadError, ItemFluidIdTable, JsonValue,
    ParsedModRecipe,
};
use crate::dimension_mod::{
    discover_dimension_mod_entries, DimensionModClassification, DimensionModData,
};
use crate::ecosystem_compat::{
    scan_mod_jar_named, JarScanError, ModEnvironment, ModJarDescriptor,
};
use crate::fallback::{is_builtin_superseded_mod, REASON_SUPERSEDED};
use crate::hard_mod_primitives::{classify_perf_or_render_mod_compat, EngineCompatDisposition};
use crate::loader_extensions::{
    biome_modifier_id_for_path, data_map_key_for_path, parse_biome_modifier, BiomeModifier,
    DataMapRegistry,
};
use crate::mixin_lifter::{
    lift_accessor_sites, LiftedBinding, MixinAnnotationKind, MixinLiftVerdict, MixinSite,
    UnliftableReason,
};
use crate::mod_router::{
    route_mod_jar, validate_member_name, ModRouteDecision, RouteMatchKind, SecurityThreat,
};
use crate::modpack::{infer_candidate_mod_id, read_zip_member, zip_member_list};
use crate::unified_bundle::{DeclarativeModBundle, BUNDLE_SCHEMA};
use crate::version_compat::{McVersion, VersionRange};

/// JAR member (archive root) carrying an embedded native
/// `mc-spine-mod-bundle/1` document for this adapter to install verbatim.
pub const EMBEDDED_BUNDLE_MEMBER: &str = "mc-spine-bundle.json";

/// Deterministic report filename written into a `--out` directory.
pub const ADAPTATION_REPORT_FILE: &str = "adaptation-report.json";

/// Per-mod bundle filename suffix under `--out`: `<mod_id>.bundle.json`.
pub const BUNDLE_FILE_SUFFIX: &str = ".bundle.json";

/// Default `mc_version_range` on synthesized and scaffolded bundles — the
/// range `unified_bundle::export_builtin_dimension_bundles` declares,
/// admitting both first-class targets (`1.21.1` and `26.3`).
const DEFAULT_BUNDLE_MC_RANGE: &str = "[1.21,)";

/// Generic `data/` extraction budgets: at most 32768 archive members to
/// consider (real production JARs exceed 12,000), 8 MiB of decoded
/// payload per member, and 128 MiB of decoded `data/` payload per JAR —
/// all enforced fail-closed.
const MAX_DATA_MEMBERS: usize = 32_768;
const MAX_DATA_MEMBER_BYTES: u64 = 8 * 1024 * 1024;
const MAX_DATA_SCAN_BYTES: u64 = 128 * 1024 * 1024;

/// Longest accepted scaffold `display_name` / `version` string.
const MAX_SCAFFOLD_FIELD_CHARS: usize = 128;

/// Options shared by [`auto_adapt_jar`] and [`auto_adapt_mods_dir`].
#[derive(Clone, Debug)]
pub struct AutoAdaptOptions {
    /// `true` (default — mc-spine is a dedicated server): mixin classes a
    /// config lists only under `client` are skipped; `false` scans them
    /// too (`--include-client-mixins`).
    pub server_only: bool,
    /// Minecraft version the server runs; a declared range that does not
    /// admit it becomes a Tier 3 blocker.
    pub target: McVersion,
    /// Mod ids a flagship native adapter owns (default: `create`).
    pub flagship_mod_ids: BTreeSet<String>,
    /// Mod ids covered by a declarative offload profile (default: the
    /// built-in reference profiles for the AE2/Mekanism/Pipez class).
    pub profile_mod_ids: BTreeSet<String>,
}

impl Default for AutoAdaptOptions {
    fn default() -> Self {
        Self {
            server_only: true,
            target: McVersion::CURRENT,
            flagship_mod_ids: ["create".to_string()].into_iter().collect(),
            profile_mod_ids: crate::profile::builtin_reference_profiles()
                .iter()
                .map(|p| p.mod_id.clone())
                .collect(),
        }
    }
}

/// The pipeline's verdict for one mod JAR.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdaptationTier {
    /// Skipped before JVM execution: a built-in superseded performance
    /// mod, a capability-routed engine mod, or a client-only mod.
    AutoSkipped,
    /// Runs fully native with zero JVM: a validated
    /// `mc-spine-mod-bundle/1` document was produced (embedded or
    /// synthesized from discovered data).
    NativeBundle,
    /// Every extracted mixin site lifted onto a standard event hook; the
    /// mod runs on the bounded JVM shim.
    Tier2LiftedShim,
    /// At least one blocker prevents native or shim execution; the
    /// `blockers` list carries the exact human-readable reasons.
    Tier3FallbackRequired,
}

impl AdaptationTier {
    /// Stable report/JSON name.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::AutoSkipped => "auto_skipped",
            Self::NativeBundle => "native_bundle",
            Self::Tier2LiftedShim => "tier2_lifted_shim",
            Self::Tier3FallbackRequired => "tier3_fallback_required",
        }
    }
}

/// Generic `data/` and bytecode surface of one JAR — the raw counts the
/// tier synthesizer and the report consume.
#[derive(Clone, Debug, Default)]
pub struct GenericDataSummary {
    /// Every `data/<ns>/` namespace, sorted.
    pub namespaces: Vec<String>,
    /// `data/<ns>/recipe(s)/*.json` files.
    pub recipe_files: usize,
    /// `data/<ns>/tags/**/*.json` files.
    pub tag_files: usize,
    /// `data/<ns>/dimension/*.json` files.
    pub dimension_files: usize,
    /// `data/<ns>/dimension_type/*.json` files.
    pub dimension_type_files: usize,
    /// `data/<ns>/loot_table(s)/*.json` files.
    pub loot_table_files: usize,
    /// Remaining `data/<ns>/**.json` files outside the named categories.
    pub other_data_files: usize,
    /// `.class` members — the JAR's Java bytecode surface.
    pub java_class_members: usize,
    /// `*.mixins.json` mixin configs found at the archive root or nested.
    pub mixin_config_files: usize,
    /// An [`EMBEDDED_BUNDLE_MEMBER`] member is present.
    pub embedded_bundle: bool,
}

/// The real parsed generic `data/` content of one mod JAR — the six
/// 1.21.1 data families [`extract_mod_data`] lifts for ANY namespace:
/// recipes, tags, loot tables, dimensions + dimension types, NeoForge
/// 1.21.1 data maps, and NeoForge 1.21.1 biome modifiers. Every entry is
/// recorded under the JAR's own `data/<ns>` namespace with zero hardcoded
/// mod ids.
///
/// Extraction is fail-closed: a corrupted archive or unsafe member path
/// aborts with a hard error, while a member that survives the bounded
/// read but fails its family's parser is recorded in
/// [`Self::rejected_members`] instead of being silently dropped.
#[derive(Clone, Debug, Default)]
pub struct ModDataBundle {
    /// Every `data/<ns>/` namespace seen, sorted.
    pub namespaces: Vec<String>,
    /// `data/<ns>/recipe/**.json` and `data/<ns>/recipes/**.json`
    /// documents that matched the generic processing recipe schema.
    pub recipes: Vec<ParsedModRecipe>,
    /// `data/<ns>/tags/**.json` (and singular `tag/`) → `ns:path` →
    /// declared value ids.
    pub tags: BTreeMap<String, Vec<String>>,
    /// `data/<ns>/loot_table/**.json` and `data/<ns>/loot_tables/**.json`
    /// → `ns:path` → the parsed document.
    pub loot_tables: BTreeMap<String, JsonValue>,
    /// `data/<ns>/dimension/**.json`, `dimension_type/**.json`, worldgen,
    /// and `tags/block(s)` members — the semantic dimension-mod scan.
    pub dimensions: DimensionModData,
    /// `data/<ns>/data_maps/**.json` and
    /// `data/<ns>/neoforge/data_maps/**.json` merged into the shared
    /// NeoForge 1.21.1 data-map registry (furnace fuels, compostables,
    /// oxidizables, and every other registered map).
    pub data_maps: DataMapRegistry,
    /// `data/<ns>/neoforge/biome_modifier/**.json` parsed documents.
    pub biome_modifiers: Vec<BiomeModifier>,
    /// `data/` members that matched a recognized family but failed to
    /// parse, recorded as `"path (reason)"` — never silently dropped.
    pub rejected_members: Vec<String>,
}

/// The artifact [`synthesize_bundle`] produces for a native JAR: the
/// validated `mc-spine-mod-bundle/1` document packaged together with the
/// real [`ModDataBundle`] extracted from the JAR's `data/` members.
#[derive(Clone, Debug)]
pub struct SynthesizedBundle {
    /// The validated bundle document.
    pub bundle: DeclarativeModBundle,
    /// The extracted generic `data/` content.
    pub data: ModDataBundle,
}

/// Complete outcome of adapting one mod JAR.
#[derive(Clone, Debug)]
pub struct JarAdaptationResult {
    /// Display name of the JAR (the `--adapt-jar` path, or the bare file
    /// name inside a `--adapt-mods-dir` run).
    pub jar: String,
    /// [`infer_candidate_mod_id`] over the file name.
    pub inferred_mod_id: Option<String>,
    /// Loader-manifest descriptor — `None` when the manifest scan failed
    /// (the failure is recorded in `blockers`) or the JAR was skipped
    /// before metadata was read.
    pub descriptor: Option<ModJarDescriptor>,
    /// The tier the JAR was assigned.
    pub tier: AdaptationTier,
    /// Static capability-routing verdict, including a security quarantine.
    pub route: ModRouteDecision,
    /// Evidence of dangerous bytecode or executable archive members.
    pub security_threats: Vec<SecurityThreat>,
    /// Integration strategy inferred from the declared mod id, falling
    /// back to the JAR name when loader metadata is unavailable.
    pub engine_compat: EngineCompatDisposition,
    /// Chronological audit trail of every pipeline decision.
    pub audit_notes: Vec<String>,
    /// Exact human-readable blockers — non-empty iff
    /// [`AdaptationTier::Tier3FallbackRequired`].
    pub blockers: Vec<String>,
    /// `.class` mixin scan — `None` when the JAR was skipped early.
    pub mixin_scan: Option<JarMixinScanReport>,
    /// Culinary discovery counters — `None` when skipped early.
    pub culinary: Option<CulinaryDiscoveryStats>,
    /// Dimension-mod data discovery — `None` when skipped early.
    pub dimensions: Option<DimensionModData>,
    /// Generic `data/` + bytecode surface counts.
    pub data: GenericDataSummary,
    /// The real parsed `data/` content extracted from the JAR — empty
    /// only when the JAR was skipped before generic discovery ran or held
    /// no `data/` members at all.
    pub data_bundle: ModDataBundle,
    /// The validated `mc-spine-mod-bundle/1` document — `Some` iff the
    /// tier is [`AdaptationTier::NativeBundle`].
    pub bundle: Option<DeclarativeModBundle>,
}

impl JarAdaptationResult {
    /// Primary mod id: the manifest-declared id when present, otherwise
    /// the filename-inferred id, otherwise `None`.
    pub fn mod_id(&self) -> Option<&str> {
        self.descriptor
            .as_ref()
            .map(|d| d.mod_id.as_str())
            .filter(|id| !id.is_empty())
            .or(self.inferred_mod_id.as_deref())
    }

    /// `true` when the tier is [`AdaptationTier::Tier3FallbackRequired`].
    pub fn requires_fallback(&self) -> bool {
        self.tier == AdaptationTier::Tier3FallbackRequired
    }

    /// Deterministic `mc-spine-mod-bundle/1` JSON for the emitted bundle.
    pub fn bundle_json(&self) -> Option<String> {
        self.bundle.as_ref().map(DeclarativeModBundle::to_json)
    }

    /// Deterministic `mc-spine-jar-adaptation/1` JSON form of the result.
    pub fn to_json(&self) -> String {
        jar_result_json(self)
    }

    /// Compact operator-facing summary: tier line, one-line data surface,
    /// then every audit note and blocker.
    pub fn format_human(&self) -> String {
        format_adapt_human(self)
    }
}

/// Outcome of adapting a whole `mods/` directory.
#[derive(Clone, Debug)]
pub struct ModsDirAdaptationReport {
    /// Directory that was scanned.
    pub dir: String,
    /// Per-JAR results in deterministic filename order.
    pub jars: Vec<JarAdaptationResult>,
    /// Non-`*.jar` entries ignored in the directory.
    pub non_jar_entries: usize,
    /// Report-level audit notes (e.g. duplicate declared mod ids).
    pub audit_notes: Vec<String>,
}

impl ModsDirAdaptationReport {
    /// Number of JARs assigned `tier`.
    pub fn tier_count(&self, tier: AdaptationTier) -> usize {
        self.jars.iter().filter(|j| j.tier == tier).count()
    }

    /// `true` when at least one JAR needs a Tier 3 fallback.
    pub fn requires_fallback(&self) -> bool {
        self.jars.iter().any(JarAdaptationResult::requires_fallback)
    }

    /// Deterministic `mc-spine-mods-dir-adaptation/1` JSON report.
    pub fn to_json(&self) -> String {
        mods_dir_report_json(self)
    }

    /// Operator-facing summary: one line per JAR, per-tier totals, then a
    /// plain-language verdict.
    pub fn format_human(&self) -> String {
        format_adapt_dir_human(self)
    }
}

/// The case/separator-insensitive comparison `--scan-mods-dir` and
/// `modpack` apply: `-`/`_` removed, lowercased — `embeddium-extra` still
/// matches the catalog's `embeddium_extra`.
fn normalized_mod_key(id: &str) -> String {
    id.chars()
        .filter(|c| !matches!(c, '-' | '_'))
        .flat_map(char::to_lowercase)
        .collect()
}

/// Whether `id` names a superseded performance mod under the normalized
/// comparison (a superset of [`is_builtin_superseded_mod`]'s exact
/// case-insensitive match).
fn superseded_id(id: &str) -> bool {
    is_builtin_superseded_mod(id)
        || crate::fallback::BUILTIN_SUPERSEDED_PERF_MODS
            .iter()
            .any(|entry| normalized_mod_key(entry) == normalized_mod_key(id))
}

/// Record the hard-mod integration strategy independently of the execution
/// tier. A superseded JAR can still require a client-side resource bridge.
fn classify_adapted_mod_compat(
    descriptor: Option<&ModJarDescriptor>,
    inferred_mod_id: Option<&str>,
    audit_notes: &mut Vec<String>,
) -> EngineCompatDisposition {
    let mod_id = descriptor
        .map(|d| d.mod_id.as_str())
        .filter(|id| !id.is_empty())
        .or(inferred_mod_id);
    let disposition = mod_id.map_or(EngineCompatDisposition::StandardModAdapter, |id| {
        let classified = classify_perf_or_render_mod_compat(id);
        if classified == EngineCompatDisposition::StandardModAdapter && descriptor.is_none() {
            // Filename inference folds underscores into hyphens; the
            // classifier retains underscores in some canonical mod ids.
            classify_perf_or_render_mod_compat(&id.replace('-', "_"))
        } else {
            classified
        }
    });
    let note = match disposition {
        EngineCompatDisposition::NativeAccelerated { native_subsystem } => {
            Some(format!("engine compatibility strategy: native_accelerated:{native_subsystem}"))
        }
        EngineCompatDisposition::NativeGameplayPrimitive { engine } => {
            Some(format!("engine compatibility strategy: native_gameplay_primitive:{engine}"))
        }
        EngineCompatDisposition::CoexistResourceBridge { bridge } => {
            Some(format!("engine compatibility strategy: coexist_resource_bridge:{bridge}"))
        }
        EngineCompatDisposition::StandardModAdapter => None,
    };
    if let Some(note) = note {
        audit_notes.push(note);
    }
    disposition
}

fn env_str(env: ModEnvironment) -> &'static str {
    match env {
        ModEnvironment::Client => "client",
        ModEnvironment::Server => "server",
        ModEnvironment::Both => "both",
    }
}

/// Count the generic `data/` + bytecode surface of one listed archive.
fn classify_members(entries: &[crate::modpack::ZipMemberEntry], data: &mut GenericDataSummary) {
    let mut namespaces = BTreeSet::new();
    for entry in entries {
        let name = entry.name.as_str();
        if name.ends_with(".class") {
            data.java_class_members += 1;
            continue;
        }
        if name.ends_with(".mixins.json") {
            data.mixin_config_files += 1;
            continue;
        }
        if name == EMBEDDED_BUNDLE_MEMBER {
            data.embedded_bundle = true;
            continue;
        }
        let Some(rest) = name.strip_prefix("data/") else {
            continue;
        };
        if !name.ends_with(".json") {
            continue;
        }
        let Some((ns, rest)) = rest.split_once('/') else {
            continue;
        };
        if ns.is_empty() {
            continue;
        }
        namespaces.insert(ns.to_string());
        match rest.split('/').next().unwrap_or("") {
            "recipe" | "recipes" => data.recipe_files += 1,
            "tags" | "tag" => data.tag_files += 1,
            "dimension" => data.dimension_files += 1,
            "dimension_type" => data.dimension_type_files += 1,
            "loot_table" | "loot_tables" => data.loot_table_files += 1,
            _ => data.other_data_files += 1,
        }
    }
    data.namespaces = namespaces.into_iter().collect();
}

/// Whether a `data/` member feeds [`discover_dimension_mod_entries`]:
/// `dimension_type`, `dimension`, `worldgen`, and `tags/{block,blocks}`.
fn dimension_member_wanted(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("data/") else {
        return false;
    };
    if !name.ends_with(".json") {
        return false;
    }
    let Some((_, rest)) = rest.split_once('/') else {
        return false;
    };
    if matches!(
        rest.split('/').next(),
        Some("dimension_type" | "dimension" | "worldgen")
    ) {
        return true;
    }
    rest.strip_prefix("tags/")
        .is_some_and(|t| matches!(t.split('/').next(), Some("block" | "blocks")))
}

/// Whether a `data/<ns>/**.json` member feeds one of the six extracted
/// data families and therefore earns a bounded read in
/// [`extract_mod_data`].
fn data_member_wanted(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("data/") else {
        return false;
    };
    if !name.ends_with(".json") {
        return false;
    }
    let Some((_, rest)) = rest.split_once('/') else {
        return false;
    };
    if dimension_member_wanted(name) {
        return true;
    }
    if matches!(
        rest.split('/').next(),
        Some("recipe" | "recipes" | "tags" | "tag" | "loot_table" | "loot_tables")
    ) {
        return true;
    }
    data_map_key_for_path(name).is_some() || biome_modifier_id_for_path(name).is_some()
}

fn data_load_error_message(e: &DataLoadError) -> &str {
    match e {
        DataLoadError::Io(m) | DataLoadError::Json(m) | DataLoadError::Zip(m) => m,
    }
}

fn reject_member(bundle: &mut ModDataBundle, name: &str, reason: impl std::fmt::Display) {
    bundle.rejected_members.push(format!("{name} ({reason})"));
}

/// Extract the six 1.21.1 generic `data/` families from one mod JAR for
/// ANY namespace — the real parsed content [`synthesize_bundle`]
/// packages into the native artifact.
///
/// Every `data/<ns>/(recipe|recipes|tags|tag|loot_table|loot_tables|
/// dimension|dimension_type|worldgen|data_maps|neoforge/data_maps|
/// neoforge/biome_modifier)/**.json` member is read through the crate's
/// bounded ZIP reader (Stored and Deflate members, per-member and total
/// byte budgets) and parsed by its family's real parser — nothing is
/// faked, nothing is keyed on a hardcoded mod id.
///
/// Fail-closed like the rest of the pipeline: a corrupt archive, an
/// unsafe (traversal) member path, or a member the bounded reader
/// rejects is a hard `Err`; a member that reads cleanly but fails its
/// family's parser is recorded in [`ModDataBundle::rejected_members`]
/// with the reason rather than silently dropped.
///
/// # Errors
///
/// `Err` on archive corruption, unsafe member paths, or a bounded-read
/// rejection — never a partial silent skip.
pub fn extract_mod_data(jar_bytes: &[u8], display: &str) -> Result<ModDataBundle, String> {
    let entries = zip_member_list(jar_bytes, display).map_err(|e| e.reason().to_string())?;
    if entries.len() > MAX_DATA_MEMBERS {
        return Err(format!(
            "{display}: data scan member count exceeds {MAX_DATA_MEMBERS}"
        ));
    }
    let mut out = ModDataBundle::default();
    let mut namespaces = BTreeSet::new();
    let mut dim_inputs: Vec<(String, Vec<u8>)> = Vec::new();
    let mut ids = ItemFluidIdTable::new();
    let mut next_recipe_id = 1u32;
    let mut extracted = 0u64;

    for entry in &entries {
        let name = entry.name.as_str();
        validate_member_name(name).map_err(|why| format!("{display}: {why}: {name}"))?;
        if !data_member_wanted(name) {
            continue;
        }
        let bytes = read_zip_member(jar_bytes, entry, display)
            .map_err(|e| e.reason().to_string())?;
        if bytes.len() as u64 > MAX_DATA_MEMBER_BYTES {
            return Err(format!(
                "{display}: data member {name} exceeds the {} MiB per-member budget",
                MAX_DATA_MEMBER_BYTES / (1024 * 1024)
            ));
        }
        extracted = extracted
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| format!("{display}: data scan byte budget overflow"))?;
        if extracted > MAX_DATA_SCAN_BYTES {
            return Err(format!("{display}: data scan byte budget exceeded"));
        }
        let Some((ns, rest)) = name
            .strip_prefix("data/")
            .and_then(|rest| rest.split_once('/'))
        else {
            continue;
        };
        if ns.is_empty() {
            continue;
        }
        namespaces.insert(ns.to_string());
        let stem = rest.strip_suffix(".json").unwrap_or(rest);
        // `ns:path-below-the-category` — `recipes/foo/bar` → `ns:foo/bar`,
        // `tags/items/veggies` → `ns:items/veggies`.
        let id = format!(
            "{ns}:{}",
            stem.split_once('/').map(|(_, tail)| tail).unwrap_or(stem)
        );
        if dimension_member_wanted(name) {
            dim_inputs.push((name.to_string(), bytes.clone()));
        }
        match rest.split('/').next().unwrap_or("") {
            "recipe" | "recipes" => {
                match parse_mod_recipe_json(&id, next_recipe_id, &bytes, &mut ids) {
                    Ok(Some(recipe)) => {
                        out.recipes.push(recipe);
                        next_recipe_id = next_recipe_id.wrapping_add(1);
                    }
                    Ok(None) => reject_member(
                        &mut out,
                        name,
                        "does not match the generic processing recipe schema",
                    ),
                    Err(e) => reject_member(
                        &mut out,
                        name,
                        format!("unparseable: {}", data_load_error_message(&e)),
                    ),
                }
            }
            "tags" | "tag" => match parse_json(&bytes) {
                Err(e) => reject_member(
                    &mut out,
                    name,
                    format!("unparseable: {}", data_load_error_message(&e)),
                ),
                Ok(doc) => {
                    let values = doc
                        .as_object()
                        .and_then(|obj| obj.get("values"))
                        .and_then(JsonValue::as_array);
                    match values {
                        None => reject_member(&mut out, name, "tag has no 'values' array"),
                        Some(values) => {
                            let mut list = Vec::with_capacity(values.len());
                            let mut bad = false;
                            for value in values {
                                let entry_id = value.as_str().or_else(|| {
                                    value
                                        .as_object()
                                        .and_then(|v| v.get("id"))
                                        .and_then(JsonValue::as_str)
                                });
                                match entry_id {
                                    Some(entry_id) => list.push(entry_id.to_string()),
                                    None => {
                                        bad = true;
                                        break;
                                    }
                                }
                            }
                            if bad {
                                reject_member(&mut out, name, "tag 'values' holds a non-id entry");
                            } else {
                                out.tags.insert(id, list);
                            }
                        }
                    }
                }
            },
            "loot_table" | "loot_tables" => match parse_json(&bytes) {
                Err(e) => reject_member(
                    &mut out,
                    name,
                    format!("unparseable: {}", data_load_error_message(&e)),
                ),
                Ok(doc) => {
                    out.loot_tables.insert(id, doc);
                }
            },
            _ => {
                if data_map_key_for_path(name).is_some() {
                    match out.data_maps.ingest_member(name, &bytes) {
                        Ok(_) => {}
                        Err(e) => reject_member(&mut out, name, format!("unparseable: {}", e.reason())),
                    }
                } else if let Some(modifier_id) = biome_modifier_id_for_path(name) {
                    match parse_biome_modifier(&modifier_id, &bytes, name) {
                        Ok(modifier) => out.biome_modifiers.push(modifier),
                        Err(e) => reject_member(&mut out, name, format!("unparseable: {}", e.reason())),
                    }
                }
            }
        }
    }
    out.namespaces = namespaces.into_iter().collect();
    out.dimensions = discover_dimension_mod_entries(
        dim_inputs.iter().map(|(p, b)| (p.as_str(), b.as_slice())),
    )
    .map_err(|e| {
        format!(
            "{display}: dimension data scan: {}",
            data_load_error_message(&e)
        )
    })?;
    Ok(out)
}

fn describe_site(site: &MixinSite) -> String {
    let at = site
        .at
        .as_ref()
        .map(|p| p.to_string())
        .unwrap_or_else(|| "<none>".to_string());
    format!(
        "{} -> {}#{} {} @At({})",
        site.mixin_class, site.target_class, site.target_method, site.annotation, at
    )
}

fn lifted_bindings(verdict: &MixinLiftVerdict) -> &[LiftedBinding] {
    match verdict {
        MixinLiftVerdict::FullyLiftable(bindings) => bindings,
        MixinLiftVerdict::RequiresFallback { lifted, .. } => lifted,
    }
}

fn unliftable_sites(verdict: &MixinLiftVerdict) -> &[(MixinSite, UnliftableReason)] {
    match verdict {
        MixinLiftVerdict::FullyLiftable(_) => &[],
        MixinLiftVerdict::RequiresFallback { unliftable, .. } => unliftable,
    }
}

fn skipped_result(
    jar: String,
    inferred_mod_id: Option<String>,
    descriptor: Option<ModJarDescriptor>,
    engine_compat: EngineCompatDisposition,
    audit_notes: Vec<String>,
    data: GenericDataSummary,
    route: ModRouteDecision,
) -> JarAdaptationResult {
    JarAdaptationResult {
        jar,
        inferred_mod_id,
        descriptor,
        tier: AdaptationTier::AutoSkipped,
        route,
        security_threats: Vec::new(),
        engine_compat,
        audit_notes,
        blockers: Vec::new(),
        mixin_scan: None,
        culinary: None,
        dimensions: None,
        data,
        data_bundle: ModDataBundle::default(),
        bundle: None,
    }
}

/// Build the native adaptation artifact for a JAR: the embedded bundle
/// verbatim when present, otherwise a synthesized empty-section
/// `mc-spine-mod-bundle/1` document carrying the mod id and its declared
/// Minecraft range (or [`DEFAULT_BUNDLE_MC_RANGE`] when undeclared) —
/// either way packaged with the real [`ModDataBundle`] the generic data
/// extraction pass lifted out of the JAR.
fn synthesize_bundle(
    embedded: Option<DeclarativeModBundle>,
    descriptor: Option<&ModJarDescriptor>,
    inferred_mod_id: Option<&str>,
    data: &mut ModDataBundle,
) -> Result<SynthesizedBundle, String> {
    let bundle = if let Some(bundle) = embedded {
        bundle
    } else {
        let mod_id = descriptor
            .map(|d| d.mod_id.as_str())
            .filter(|id| !id.is_empty())
            .or(inferred_mod_id)
            .unwrap_or("unnamed_pack");
        if !is_safe_mod_id(mod_id) {
            return Err(format!(
                "mod id '{mod_id}' is not a safe bare bundle id"
            ));
        }
        let mc_version_range = descriptor
            .and_then(|d| d.declared_mc_range.as_ref().map(|r| r.declared().to_string()))
            .unwrap_or_else(|| DEFAULT_BUNDLE_MC_RANGE.to_string());
        let bundle = DeclarativeModBundle {
            mod_id: mod_id.to_string(),
            mc_version_range,
            portals: Vec::new(),
            progression_barriers: Vec::new(),
            biome_regions: Vec::new(),
            creatures: Vec::new(),
            custom_channels: Vec::new(),
            spells: Vec::new(),
            custom_statuses: Vec::new(),
            rituals: Vec::new(),
        };
        bundle
            .validate()
            .map_err(|e| format!("synthesized bundle failed validation: {e}"))?;
        bundle
    };
    Ok(SynthesizedBundle {
        bundle,
        data: std::mem::take(data),
    })
}

/// Run the full zero-JVM adaptation pipeline over one mod JAR read from
/// `path`.
///
/// # Errors
///
/// `Err` only when the input itself is unreadable or unsafe — an unreadable
/// file, a corrupt/encrypted/hostile archive, or a member the fail-closed
/// readers reject. A mod that merely cannot adapt lands in
/// [`AdaptationTier::Tier3FallbackRequired`] with its blockers; it is
/// never an error.
pub fn auto_adapt_jar(path: &Path, opts: &AutoAdaptOptions) -> Result<JarAdaptationResult, String> {
    let display = path.display().to_string();
    let bytes =
        std::fs::read(path).map_err(|e| format!("{display}: cannot read JAR: {e}"))?;
    auto_adapt_jar_bytes(&bytes, &display, opts)
}

/// [`auto_adapt_jar`] over JAR bytes already in memory, with `display` as
/// the operator-facing name in reports and errors.
///
/// # Errors
///
/// Same fail-closed rules as [`auto_adapt_jar`].
pub fn auto_adapt_jar_bytes(
    jar_bytes: &[u8],
    display: &str,
    opts: &AutoAdaptOptions,
) -> Result<JarAdaptationResult, String> {
    let mut audit_notes = Vec::new();
    let file_name = display.rsplit('/').next().unwrap_or(display);
    let inferred_mod_id = infer_candidate_mod_id(file_name);

    // 1. Every valid archive is statically security-scanned before any
    //    filename/manifest shortcut. A corrupt archive fails closed even
    //    when named after a known superseded mod.
    let mut routing = route_mod_jar(jar_bytes, display, inferred_mod_id.as_deref())?;
    if let ModRouteDecision::SecurityQuarantine { threats } = &routing.decision {
        let blockers = threats
            .iter()
            .map(|t| {
                format!(
                    "SECURITY_QUARANTINE: {} in {} ({}) — execution and delegation prohibited",
                    t.kind.as_str(),
                    t.member,
                    t.indicator
                )
            })
            .collect();
        return Ok(JarAdaptationResult {
            jar: display.to_string(),
            inferred_mod_id,
            descriptor: None,
            tier: AdaptationTier::Tier3FallbackRequired,
            route: routing.decision.clone(),
            security_threats: threats.clone(),
            engine_compat: EngineCompatDisposition::StandardModAdapter,
            audit_notes: vec!["SECURITY_QUARANTINE: static security scan blocked execution, native adaptation, and JVM fallback".to_string()],
            blockers,
            mixin_scan: None,
            culinary: None,
            dimensions: None,
            data: GenericDataSummary::default(),
            data_bundle: ModDataBundle::default(),
            bundle: None,
        });
    }
    if let ModRouteDecision::RouteToNativeSubsystem {
        subsystem,
        matched_by: RouteMatchKind::KnownCatalogId,
        ..
    } = routing.decision.clone()
    {
        let existing =
            classify_adapted_mod_compat(None, inferred_mod_id.as_deref(), &mut audit_notes);
        let engine_compat = if existing == EngineCompatDisposition::StandardModAdapter {
            EngineCompatDisposition::NativeAccelerated {
                native_subsystem: subsystem.as_str(),
            }
        } else {
            existing
        };
        audit_notes.push(format!(
            "filename-inferred mod id matches BUILTIN_SUPERSEDED_PERF_MODS ({REASON_SUPERSEDED}); static security scan passed before superseded skip"
        ));
        return Ok(skipped_result(
            display.to_string(),
            inferred_mod_id,
            None,
            engine_compat,
            audit_notes,
            GenericDataSummary::default(),
            routing.decision,
        ));
    }

    // 2. Fail-closed member listing (traversal, bomb, truncation, CRC).
    let entries =
        zip_member_list(jar_bytes, display).map_err(|e| e.reason().to_string())?;
    let mut data = GenericDataSummary::default();
    classify_members(&entries, &mut data);

    // 3. Loader metadata. A malformed/absent manifest is not fatal yet —
    //    it becomes a Tier 3 blocker after the rest of the evidence is in.
    let mut descriptor = None;
    let mut manifest_error = None;
    match scan_mod_jar_named(jar_bytes, display) {
        Ok(d) => descriptor = Some(d),
        Err(JarScanError::Zip(msg)) => return Err(format!("{display}: {msg}")),
        Err(e) => manifest_error = Some(e.to_string()),
    }
    match &descriptor {
        Some(d) => audit_notes.push(format!(
            "loader manifest: {} loader, mod id '{}', version '{}', environment {}, {} declared mod id(s), {} mixin config(s), {} dependenc{}",
            d.loader,
            d.mod_id,
            d.version,
            env_str(d.environment),
            d.declared_mod_ids.len(),
            d.mixin_configs.len(),
            d.dependencies.len(),
            if d.dependencies.len() == 1 { "y" } else { "ies" },
        )),
        None => audit_notes.push(format!(
            "loader manifest unusable: {}",
            manifest_error.as_deref().unwrap_or("<unknown>")
        )),
    }
    let engine_compat =
        classify_adapted_mod_compat(descriptor.as_ref(), inferred_mod_id.as_deref(), &mut audit_notes);

    // 4. Resolve renamed known mods from their declared IDs. The security
    //    scan was already completed before inspecting loader metadata.
    if let Some(d) = &descriptor {
        if let Some(hit) = d.declared_mod_ids.iter().find(|id| superseded_id(id)) {
            routing = route_mod_jar(jar_bytes, display, Some(hit))?;
            if let ModRouteDecision::RouteToNativeSubsystem { subsystem, .. } = &routing.decision {
                audit_notes.push(format!(
                    "declared mod id '{hit}' matches BUILTIN_SUPERSEDED_PERF_MODS ({REASON_SUPERSEDED}); security scan passed before superseded skip"
                ));
                let declared_strategy = classify_perf_or_render_mod_compat(hit);
                let strategy = if declared_strategy == EngineCompatDisposition::StandardModAdapter {
                    EngineCompatDisposition::NativeAccelerated {
                        native_subsystem: subsystem.as_str(),
                    }
                } else {
                    declared_strategy
                };
                return Ok(skipped_result(
                    display.to_string(),
                    inferred_mod_id,
                    descriptor,
                    strategy,
                    audit_notes,
                    data,
                    routing.decision,
                ));
            }
        }
    }

    if let ModRouteDecision::RouteToNativeSubsystem {
        subsystem,
        matched_by: RouteMatchKind::BehavioralCapability,
        evidence,
    } = routing.decision.clone()
    {
        audit_notes.push(format!(
            "unknown engine-replacement mod routed generically by structural capability signature ({}) to native {} without executing Java bytecode",
            evidence.join(", "), subsystem.as_str()
        ));
        return Ok(skipped_result(
            display.to_string(),
            inferred_mod_id,
            descriptor,
            EngineCompatDisposition::NativeAccelerated {
                native_subsystem: subsystem.as_str(),
            },
            audit_notes,
            data,
            routing.decision,
        ));
    }

    if let ModRouteDecision::HybridSplit {
        client_render_fallback,
        evidence,
    } = &routing.decision
    {
        audit_notes.push(format!(
            "hybrid mod: client rendering uses native {} ({}); gameplay continues through the standard native/shim adaptation pipeline",
            client_render_fallback.as_str(), evidence.join(", ")
        ));
    }

    // 5. Client-only manifests never stage on a dedicated server.
    if matches!(
        descriptor.as_ref().map(|d| d.environment),
        Some(ModEnvironment::Client)
    ) {
        audit_notes.push(
            "declares a client-only environment; never staged on a dedicated server".to_string(),
        );
        return Ok(skipped_result(
            display.to_string(),
            inferred_mod_id,
            descriptor,
            engine_compat,
            audit_notes,
            data,
            routing.decision,
        ));
    }

    // 6. Generic data discovery: the real parsed content of every 1.21.1
    //    `data/` family (recipes, tags, loot tables, dimensions +
    //    dimension types, NeoForge data maps, NeoForge biome modifiers)
    //    for ANY namespace — bounded reads, fail-closed on corrupted or
    //    traversal entries — plus the embedded bundle member.
    let mut data_bundle = extract_mod_data(jar_bytes, display)?;
    let mut embedded_bundle_bytes = None;
    for entry in &entries {
        if entry.name == EMBEDDED_BUNDLE_MEMBER {
            embedded_bundle_bytes = Some(
                read_zip_member(jar_bytes, entry, display)
                    .map_err(|e| e.reason().to_string())?,
            );
            break;
        }
    }
    let dimensions = data_bundle.dimensions.clone();
    {
        let extracted_families = data_bundle.recipes.len()
            + data_bundle.tags.len()
            + data_bundle.loot_tables.len()
            + data_bundle.dimensions.dimensions.len()
            + data_bundle.dimensions.dimension_types.len()
            + data_bundle.data_maps.keys().len()
            + data_bundle.biome_modifiers.len();
        if extracted_families > 0 || !data_bundle.rejected_members.is_empty() {
            audit_notes.push(format!(
                "generic data extraction: {} recipe(s), {} tag(s), {} loot table(s), {} dimension(s), {} dimension type(s), {} NeoForge data map(s), {} biome modifier(s), {} rejected member(s)",
                data_bundle.recipes.len(),
                data_bundle.tags.len(),
                data_bundle.loot_tables.len(),
                data_bundle.dimensions.dimensions.len(),
                data_bundle.dimensions.dimension_types.len(),
                data_bundle.data_maps.keys().len(),
                data_bundle.biome_modifiers.len(),
                data_bundle.rejected_members.len(),
            ));
        }
    }
    if dimensions.is_dimension_mod() || !dimensions.dimension_types.is_empty() {
        audit_notes.push(format!(
            "dimension data: {} dimension type(s), {} dimension(s), {} worldgen declaration(s){}",
            dimensions.dimension_types.len(),
            dimensions.dimensions.len(),
            dimensions.worldgen.noise_settings.len()
                + dimensions.worldgen.biomes.len()
                + dimensions.worldgen.configured_features.len()
                + dimensions.worldgen.placed_features.len()
                + dimensions.worldgen.structures.len(),
            if matches!(
                dimensions.classification(),
                DimensionModClassification::PureDataDriven
            ) {
                ", pure data-driven"
            } else {
                ", carries a custom chunk generator"
            },
        ));
    }

    // 7. Embedded native bundle validation.
    let mut embedded_bundle = None;
    let mut embedded_bundle_error = None;
    if let Some(bytes) = embedded_bundle_bytes {
        let text = String::from_utf8(bytes).map_err(|_| {
            format!("{display}: {EMBEDDED_BUNDLE_MEMBER} is not UTF-8")
        })?;
        match DeclarativeModBundle::from_json(&text) {
            Ok(b) => {
                audit_notes.push(format!(
                    "embedded {EMBEDDED_BUNDLE_MEMBER} validated as {BUNDLE_SCHEMA} for mod id '{}'",
                    b.mod_id
                ));
                embedded_bundle = Some(b);
            }
            Err(e) => {
                embedded_bundle_error = Some(e.to_string());
            }
        }
    }

    // 8. Generic culinary discovery (cooking/cutting recipes, tags, loot
    //    modifiers, food specs) — one registry per JAR keeps the stats
    //    strictly per-mod.
    let culinary = {
        let mut registry = CulinaryRegistry::new();
        registry
            .load_from_jar_bytes(jar_bytes)
            .map_err(|e| format!("{display}: culinary discovery: {e}"))?
    };
    let culinary_hits = culinary.cooking_recipes
        + culinary.cutting_recipes
        + culinary.loot_modifiers
        + culinary.food_specs;
    if culinary_hits > 0 {
        audit_notes.push(format!(
            "culinary discovery: {} cooking, {} cutting, {} loot modifier(s), {} food spec(s)",
            culinary.cooking_recipes,
            culinary.cutting_recipes,
            culinary.loot_modifiers,
            culinary.food_specs,
        ));
    }

    // 9. `.class` mixin bytecode scan + lift.
    let mixin_scan = scan_jar_mixins_with_options(
        jar_bytes,
        JarScanOptions {
            server_only: opts.server_only,
        },
    )
    .map_err(|e| format!("{display}: mixin scan: {e}"))?;
    audit_notes.push(format!(
        "mixin bytecode scan: {} config(s), {} class(es) scanned, {} site(s), {} client-only class(es) skipped",
        mixin_scan.mixin_configs_found,
        mixin_scan.classes_scanned,
        mixin_scan.sites.len(),
        mixin_scan.skipped_client_mixins,
    ));

    // 10. Tier synthesis.
    let mut blockers = Vec::new();
    if let Some(e) = manifest_error {
        if embedded_bundle.is_none() {
            blockers.push(format!("no usable loader manifest ({e})"));
        }
    }
    if let Some(d) = &descriptor {
        if let Some(range) = &d.declared_mc_range {
            if !range.contains(opts.target) {
                blockers.push(format!(
                    "declares Minecraft range '{}', which does not admit server {}",
                    range.declared(),
                    opts.target
                ));
            }
        }
        if d.extensions.requires_jvm_bridge() {
            blockers.push(
                "loader extensions require a JVM bridge (embedded Jar-in-Jar contents and/or access transformer directives)"
                    .to_string(),
            );
        }
    }
    if let Some(e) = embedded_bundle_error {
        blockers.push(format!(
            "embedded {EMBEDDED_BUNDLE_MEMBER} failed {BUNDLE_SCHEMA} validation: {e}"
        ));
    }
    if let Some(b) = &embedded_bundle {
        if !is_safe_mod_id(&b.mod_id) {
            blockers.push(format!(
                "embedded bundle declares unsafe mod id '{}'",
                b.mod_id
            ));
        }
        if let Ok(range) = VersionRange::parse(&b.mc_version_range) {
            if !range.contains(opts.target) {
                blockers.push(format!(
                    "embedded bundle declares mc_version_range '{}', which does not admit server {}",
                    b.mc_version_range, opts.target
                ));
            }
        }
    }
    if let DimensionModClassification::CustomChunkGenerator(kinds) =
        dimensions.classification()
    {
        blockers.push(format!(
            "dimension data uses a custom Java chunk generator ({})",
            kinds.join(", ")
        ));
    }
    // Read-only `@Accessor` analysis: safe getters lift onto native field
    // reads; setters and anything unverifiable stay unliftable carrying
    // their own structured reason (never a silent drop).
    let accessor_plan = lift_accessor_sites(&mixin_scan.sites);
    for (site, reason) in unliftable_sites(&mixin_scan.verdict) {
        if site.annotation == MixinAnnotationKind::Accessor {
            continue;
        }
        blockers.push(format!(
            "mixin site {} stays unliftable ({})",
            describe_site(site),
            reason
        ));
    }
    for (site, reason) in &accessor_plan.rejected {
        blockers.push(format!(
            "@Accessor site {} cannot lift natively ({})",
            describe_site(site),
            reason.as_str()
        ));
    }
    if !accessor_plan.bindings.is_empty() {
        let mut fields: Vec<&str> = accessor_plan
            .bindings
            .iter()
            .map(|b| b.field.as_str())
            .collect();
        fields.sort_unstable();
        fields.dedup();
        audit_notes.push(format!(
            "read-only @Accessor lifting: {} field read(s) lifted natively ({})",
            accessor_plan.bindings.len(),
            fields.join(", ")
        ));
    }

    let native_surface = embedded_bundle.is_some()
        || descriptor.as_ref().is_some_and(|d| {
            opts.flagship_mod_ids.contains(&d.mod_id)
                || opts.profile_mod_ids.contains(&d.mod_id)
        })
        || culinary_hits > 0
        || dimensions.is_dimension_mod()
        || !dimensions.dimension_types.is_empty()
        || data.java_class_members == 0
        || matches!(routing.decision, ModRouteDecision::HybridSplit { .. });

    let (tier, bundle) = if !blockers.is_empty() {
        (AdaptationTier::Tier3FallbackRequired, None)
    } else if !mixin_scan.sites.is_empty() {
        let lifted = lifted_bindings(&mixin_scan.verdict);
        let mut hooks: Vec<&'static str> =
            lifted.iter().map(|b| b.hook.as_str()).collect();
        hooks.sort_unstable();
        hooks.dedup();
        if accessor_plan.bindings.is_empty() {
            audit_notes.push(format!(
                "all {} mixin site(s) lifted onto standard event hooks ({})",
                lifted.len(),
                hooks.join(", ")
            ));
        } else {
            let mut detail = hooks.join(", ");
            if !detail.is_empty() {
                detail.push_str(", ");
            }
            detail.push_str(&format!(
                "{} read-only accessor field read(s)",
                accessor_plan.bindings.len()
            ));
            audit_notes.push(format!(
                "all {} mixin site(s) lifted ({})",
                lifted.len() + accessor_plan.bindings.len(),
                detail
            ));
        }
        (AdaptationTier::Tier2LiftedShim, None)
    } else if native_surface {
        match synthesize_bundle(
            embedded_bundle,
            descriptor.as_ref(),
            inferred_mod_id.as_deref(),
            &mut data_bundle,
        ) {
            Ok(syn) => {
                audit_notes.push(format!(
                    "synthesized {BUNDLE_SCHEMA} for mod id '{}' (mc_version_range '{}')",
                    syn.bundle.mod_id, syn.bundle.mc_version_range
                ));
                data_bundle = syn.data;
                (AdaptationTier::NativeBundle, Some(syn.bundle))
            }
            Err(e) => {
                blockers.push(e);
                (AdaptationTier::Tier3FallbackRequired, None)
            }
        }
    } else {
        blockers.push(format!(
            "contains Java bytecode ({} class member(s)) with no liftable mixin sites and no discoverable declarative content",
            data.java_class_members
        ));
        (AdaptationTier::Tier3FallbackRequired, None)
    };

    Ok(JarAdaptationResult {
        jar: display.to_string(),
        inferred_mod_id,
        descriptor,
        tier,
        route: routing.decision,
        security_threats: Vec::new(),
        engine_compat,
        audit_notes,
        blockers,
        mixin_scan: Some(mixin_scan),
        culinary: Some(culinary),
        dimensions: Some(dimensions),
        data,
        data_bundle,
        bundle,
    })
}

/// Adapt every `*.jar` in `dir`, in deterministic filename order (the same
/// case-insensitive sort `--scan-mods-dir` uses). Any unreadable entry or
/// JAR fails the whole pass — a half-adapted directory looks identical to
/// a clean one otherwise.
///
/// # Errors
///
/// `Err` when `dir` is missing/unreadable or any JAR inside fails the
/// fail-closed archive checks (see [`auto_adapt_jar`]).
pub fn auto_adapt_mods_dir(
    dir: &Path,
    opts: &AutoAdaptOptions,
) -> Result<ModsDirAdaptationReport, String> {
    if !dir.is_dir() {
        return Err(format!(
            "'{}': not an existing directory",
            dir.display()
        ));
    }
    let entries = std::fs::read_dir(dir).map_err(|e| {
        format!("'{}': cannot read mods directory: {e}", dir.display())
    })?;
    let mut names = Vec::new();
    let mut non_jar = 0usize;
    for entry in entries {
        let entry = entry.map_err(|e| {
            format!("'{}': cannot list directory entry: {e}", dir.display())
        })?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.to_ascii_lowercase().ends_with(".jar") {
            names.push(name);
        } else {
            non_jar += 1;
        }
    }
    names.sort_by(|a, b| {
        a.to_lowercase()
            .cmp(&b.to_lowercase())
            .then_with(|| a.cmp(b))
    });

    let mut jars = Vec::with_capacity(names.len());
    let mut seen_ids: BTreeSet<String> = BTreeSet::new();
    let mut report_notes = Vec::new();
    for name in names {
        let path = dir.join(&name);
        let bytes = std::fs::read(&path)
            .map_err(|e| format!("'{name}': cannot read JAR: {e}"))?;
        let result = auto_adapt_jar_bytes(&bytes, &name, opts)
            .map_err(|e| format!("'{name}': {e}"))?;
        if let Some(id) = result.mod_id() {
            let key = normalized_mod_key(id);
            if !seen_ids.insert(key) {
                report_notes.push(format!(
                    "'{name}' declares mod id '{id}', which another JAR in this directory also declares"
                ));
            }
        }
        jars.push(result);
    }
    Ok(ModsDirAdaptationReport {
        dir: dir.display().to_string(),
        jars,
        non_jar_entries: non_jar,
        audit_notes: report_notes,
    })
}

/// A mod id safe to use as a bare bundle id and output file name:
/// non-empty, `[a-z0-9_.-]` only (no separators, `:` or whitespace), not
/// a leading/all-dot traversal shape, and parseable as a resource
/// location.
fn is_safe_mod_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '.' | '-'))
        && !id.starts_with('.')
        && id.chars().any(|c| c != '.')
        && ResourceLocation::parse(id).is_ok()
}

/// Validate `mod_id` as a bare bundle/file id and return its
/// `<mod_id>.bundle.json` file name. Ids carrying characters outside
/// `[a-z0-9_.-]` (path separators, `:`, whitespace, …) or a leading `.`
/// are rejected rather than silently renamed — a hostile or malformed id
/// must never pick its own output path.
pub fn bundle_file_name(mod_id: &str) -> Result<String, String> {
    if !is_safe_mod_id(mod_id) {
        return Err(format!("unsafe bundle mod id '{mod_id}'"));
    }
    Ok(format!("{mod_id}{BUNDLE_FILE_SUFFIX}"))
}

/// Persist adaptation output into `out_dir` (created recursively):
/// `<mod_id>.bundle.json` for every JAR that produced a validated bundle,
/// then the deterministic [`ADAPTATION_REPORT_FILE`]. Unsafe mod ids
/// reject the write rather than pick a different name.
///
/// Returns every written path in write order (bundles in `jars` order,
/// the report last).
///
/// # Errors
///
/// `Err` on any unsafe mod id or filesystem failure; earlier writes are
/// left in place (they are complete, deterministic documents).
pub fn write_adaptation_outputs(
    out_dir: &Path,
    report_json: &str,
    jars: &[JarAdaptationResult],
) -> Result<Vec<PathBuf>, String> {
    std::fs::create_dir_all(out_dir).map_err(|e| {
        format!(
            "'{}': cannot create output directory: {e}",
            out_dir.display()
        )
    })?;
    let mut written = Vec::new();
    for jar in jars {
        let Some(bundle) = &jar.bundle else {
            continue;
        };
        let name = bundle_file_name(&bundle.mod_id)?;
        let path = out_dir.join(&name);
        std::fs::write(&path, bundle.to_json())
            .map_err(|e| format!("'{}': cannot write bundle: {e}", path.display()))?;
        written.push(path);
    }
    let report_path = out_dir.join(ADAPTATION_REPORT_FILE);
    std::fs::write(&report_path, report_json).map_err(|e| {
        format!(
            "'{}': cannot write {ADAPTATION_REPORT_FILE}: {e}",
            report_path.display()
        )
    })?;
    written.push(report_path);
    Ok(written)
}

/// Emit a starter `mc-spine-mod-bundle/1` document for a native zero-JVM
/// mod. `mod_id` must be a bare resource-location id (`[a-z0-9_.-]`, no
/// `:`) — anything else is rejected. `version`, when non-empty, becomes
/// the bundle's `mc_version_range` and must parse as a Maven or SemVer
/// range; empty defaults to `[1.21,)`, admitting both first-class
/// targets. `display_name` is validated and returned to the caller for
/// its report — the v1 bundle schema is intentionally closed (unknown
/// keys are rejected), so a display name cannot ride inside the document;
/// the emitted JSON always round-trips through
/// [`DeclarativeModBundle::from_json`].
///
/// # Errors
///
/// `Err` on an invalid mod id, an over-length or control-character
/// `display_name`, or an unparsable `version` range.
pub fn scaffold_native_bundle(
    mod_id: &str,
    display_name: &str,
    version: &str,
) -> Result<String, String> {
    let id = mod_id.trim();
    if !is_safe_mod_id(id) {
        return Err(format!(
            "invalid mod id '{id}': expected a bare resource-location id \
             ([a-z0-9_.-], no 'namespace:' prefix or separators)"
        ));
    }

    let name = display_name.trim();
    if name.chars().count() > MAX_SCAFFOLD_FIELD_CHARS
        || name.chars().any(|c| c.is_control())
    {
        return Err(format!(
            "display name must be at most {MAX_SCAFFOLD_FIELD_CHARS} printable characters"
        ));
    }

    let version = version.trim();
    let mc_version_range = if version.is_empty() {
        DEFAULT_BUNDLE_MC_RANGE.to_string()
    } else {
        VersionRange::parse(version).map_err(|e| {
            format!("invalid version range '{version}': {}", e.reason())
        })?;
        version.to_string()
    };

    let bundle = DeclarativeModBundle {
        mod_id: id.to_string(),
        mc_version_range,
        portals: Vec::new(),
        progression_barriers: Vec::new(),
        biome_regions: Vec::new(),
        creatures: Vec::new(),
        custom_channels: Vec::new(),
        spells: Vec::new(),
        custom_statuses: Vec::new(),
        rituals: Vec::new(),
    };
    bundle
        .validate()
        .map_err(|e| format!("scaffolded bundle failed validation: {e}"))?;
    Ok(bundle.to_json())
}

// ---------------------------------------------------------------------------
// Deterministic JSON report emitters (same line shape as `modpack`'s
// pre-flight JSON: json.dumps(indent=2)-compatible, escaped ensure_ascii)
// ---------------------------------------------------------------------------

fn jesc(out: &mut String, s: &str) {
    use std::fmt::Write;
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (' '..='~').contains(&c) => out.push(c),
            c => {
                let mut buf = [0u16; 2];
                for u in c.encode_utf16(&mut buf) {
                    let _ = write!(out, "\\u{u:04x}");
                }
            }
        }
    }
    out.push('"');
}

fn jf_str(out: &mut String, level: usize, key: &str, value: &str, comma: bool) {
    use std::fmt::Write;
    let _ = write!(out, "{:width$}\"{key}\": ", "", width = level * 2);
    jesc(out, value);
    let _ = writeln!(out, "{}", if comma { "," } else { "" });
}

fn jf_opt(out: &mut String, level: usize, key: &str, value: Option<&str>, comma: bool) {
    use std::fmt::Write;
    let _ = write!(out, "{:width$}\"{key}\": ", "", width = level * 2);
    match value {
        Some(v) => jesc(out, v),
        None => out.push_str("null"),
    }
    let _ = writeln!(out, "{}", if comma { "," } else { "" });
}

fn jf_u64(out: &mut String, level: usize, key: &str, value: u64, comma: bool) {
    use std::fmt::Write;
    let _ = writeln!(
        out,
        "{:width$}\"{key}\": {value}{}",
        "",
        if comma { "," } else { "" },
        width = level * 2
    );
}

fn jf_bool(out: &mut String, level: usize, key: &str, value: bool, comma: bool) {
    use std::fmt::Write;
    let _ = writeln!(
        out,
        "{:width$}\"{key}\": {value}{}",
        "",
        if comma { "," } else { "" },
        width = level * 2
    );
}

/// `"key": ["a", "b"]` on one line when empty, multi-line otherwise.
fn jf_str_list(out: &mut String, level: usize, key: &str, items: &[String], comma: bool) {
    use std::fmt::Write;
    let _ = write!(out, "{:width$}\"{key}\": ", "", width = level * 2);
    if items.is_empty() {
        out.push_str("[]");
        let _ = writeln!(out, "{}", if comma { "," } else { "" });
        return;
    }
    out.push_str("[\n");
    for (i, item) in items.iter().enumerate() {
        let _ = write!(out, "{:width$}", "", width = (level + 1) * 2);
        jesc(out, item);
        let _ = writeln!(out, "{}", if i + 1 == items.len() { "" } else { "," });
    }
    let _ = write!(out, "{:width$}]", "", width = level * 2);
    let _ = writeln!(out, "{}", if comma { "," } else { "" });
}

/// The `mixin_class`/`target_*`/`annotation`/`at` fields shared by lifted
/// and unliftable mixin site entries.
fn json_site_fields(out: &mut String, level: usize, site: &MixinSite) {
    jf_str(out, level, "mixin_class", &site.mixin_class, true);
    jf_str(out, level, "target_class", &site.target_class, true);
    jf_str(out, level, "target_method", &site.target_method, true);
    jf_str(out, level, "annotation", site.annotation.as_str(), true);
    jf_opt(
        out,
        level,
        "at",
        site.at.as_ref().map(|p| p.to_string()).as_deref(),
        true,
    );
}

fn json_mixin_scan(out: &mut String, level: usize, scan: &JarMixinScanReport, comma: bool) {
    use std::fmt::Write;
    let _ = writeln!(out, "{:width$}\"mixin_scan\": {{", "", width = level * 2);
    jf_u64(
        out,
        level + 1,
        "mixin_configs_found",
        scan.mixin_configs_found as u64,
        true,
    );
    jf_u64(
        out,
        level + 1,
        "classes_scanned",
        scan.classes_scanned as u64,
        true,
    );
    jf_u64(
        out,
        level + 1,
        "skipped_client_mixins",
        scan.skipped_client_mixins as u64,
        true,
    );
    jf_u64(
        out,
        level + 1,
        "sites_found",
        scan.sites.len() as u64,
        true,
    );
    jf_str(
        out,
        level + 1,
        "verdict",
        match &scan.verdict {
            MixinLiftVerdict::FullyLiftable(_) => "fully_liftable",
            MixinLiftVerdict::RequiresFallback { .. } => "requires_fallback",
        },
        true,
    );

    let lifted = lifted_bindings(&scan.verdict);
    if lifted.is_empty() {
        let _ = writeln!(out, "{:width$}\"lifted_hooks\": [],", "", width = (level + 1) * 2);
    } else {
        let _ = writeln!(out, "{:width$}\"lifted_hooks\": [", "", width = (level + 1) * 2);
        for (i, binding) in lifted.iter().enumerate() {
            let last = i + 1 == lifted.len();
            let _ = writeln!(out, "{:width$}{{", "", width = (level + 2) * 2);
            jf_str(out, level + 3, "hook", binding.hook.as_str(), true);
            json_site_fields(out, level + 3, &binding.site);
            let _ = writeln!(
                out,
                "{:width$}\"cancellable\": {}",
                "",
                binding.cancellable,
                width = (level + 3) * 2
            );
            let _ = writeln!(
                out,
                "{:width$}}}{}",
                "",
                if last { "" } else { "," },
                width = (level + 2) * 2
            );
        }
        let _ = writeln!(out, "{:width$}],", "", width = (level + 1) * 2);
    }

    let unliftable = unliftable_sites(&scan.verdict);
    if unliftable.is_empty() {
        let _ = writeln!(out, "{:width$}\"unliftable\": []", "", width = (level + 1) * 2);
    } else {
        let _ = writeln!(out, "{:width$}\"unliftable\": [", "", width = (level + 1) * 2);
        for (i, (site, reason)) in unliftable.iter().enumerate() {
            let last = i + 1 == unliftable.len();
            let _ = writeln!(out, "{:width$}{{", "", width = (level + 2) * 2);
            json_site_fields(out, level + 3, site);
            jf_str(out, level + 3, "reason", reason.as_str(), false);
            let _ = writeln!(
                out,
                "{:width$}}}{}",
                "",
                if last { "" } else { "," },
                width = (level + 2) * 2
            );
        }
        let _ = writeln!(out, "{:width$}]", "", width = (level + 1) * 2);
    }
    let _ = writeln!(
        out,
        "{:width$}}}{}",
        "",
        if comma { "," } else { "" },
        width = level * 2
    );
}

fn json_data_summary(out: &mut String, level: usize, data: &GenericDataSummary) {
    use std::fmt::Write;
    let _ = writeln!(out, "{:width$}\"data\": {{", "", width = level * 2);
    jf_str_list(out, level + 1, "namespaces", &data.namespaces, true);
    jf_u64(out, level + 1, "recipe_files", data.recipe_files as u64, true);
    jf_u64(out, level + 1, "tag_files", data.tag_files as u64, true);
    jf_u64(out, level + 1, "dimension_files", data.dimension_files as u64, true);
    jf_u64(
        out,
        level + 1,
        "dimension_type_files",
        data.dimension_type_files as u64,
        true,
    );
    jf_u64(
        out,
        level + 1,
        "loot_table_files",
        data.loot_table_files as u64,
        true,
    );
    jf_u64(
        out,
        level + 1,
        "other_data_files",
        data.other_data_files as u64,
        true,
    );
    jf_u64(
        out,
        level + 1,
        "java_class_members",
        data.java_class_members as u64,
        true,
    );
    jf_u64(
        out,
        level + 1,
        "mixin_config_files",
        data.mixin_config_files as u64,
        true,
    );
    jf_bool(
        out,
        level + 1,
        "embedded_bundle",
        data.embedded_bundle,
        false,
    );
    let _ = writeln!(out, "{:width$}}},", "", width = level * 2);
}

/// The `"data_bundle"` section: per-family id lists for the real parsed
/// generic `data/` content [`extract_mod_data`] lifted out of the JAR.
fn json_data_bundle(out: &mut String, level: usize, data: &ModDataBundle) {
    use std::fmt::Write;
    let _ = writeln!(out, "{:width$}\"data_bundle\": {{", "", width = level * 2);
    jf_str_list(out, level + 1, "namespaces", &data.namespaces, true);
    jf_str_list(
        out,
        level + 1,
        "recipes",
        &data
            .recipes
            .iter()
            .map(|r| r.name.clone())
            .collect::<Vec<_>>(),
        true,
    );
    jf_str_list(
        out,
        level + 1,
        "tags",
        &data.tags.keys().cloned().collect::<Vec<_>>(),
        true,
    );
    jf_str_list(
        out,
        level + 1,
        "loot_tables",
        &data.loot_tables.keys().cloned().collect::<Vec<_>>(),
        true,
    );
    jf_str_list(
        out,
        level + 1,
        "dimensions",
        &data
            .dimensions
            .dimensions
            .iter()
            .map(|d| d.id.clone())
            .collect::<Vec<_>>(),
        true,
    );
    jf_str_list(
        out,
        level + 1,
        "dimension_types",
        &data
            .dimensions
            .dimension_types
            .iter()
            .map(|d| d.id.clone())
            .collect::<Vec<_>>(),
        true,
    );
    jf_str_list(
        out,
        level + 1,
        "data_maps",
        &data
            .data_maps
            .keys()
            .iter()
            .map(|(registry, map)| format!("{registry}/{map}"))
            .collect::<Vec<_>>(),
        true,
    );
    jf_str_list(
        out,
        level + 1,
        "biome_modifiers",
        &data
            .biome_modifiers
            .iter()
            .map(|m| m.id.clone())
            .collect::<Vec<_>>(),
        true,
    );
    jf_str_list(out, level + 1, "rejected_members", &data.rejected_members, false);
    let _ = writeln!(out, "{:width$}}},", "", width = level * 2);
}

fn jar_result_json(result: &JarAdaptationResult) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(1024);
    out.push_str("{\n");
    jf_str(&mut out, 1, "schema", "mc-spine-jar-adaptation/1", true);
    jf_str(&mut out, 1, "jar", &result.jar, true);
    jf_opt(
        &mut out,
        1,
        "inferred_mod_id",
        result.inferred_mod_id.as_deref(),
        true,
    );
    jf_opt(&mut out, 1, "mod_id", result.mod_id(), true);
    jf_str(&mut out, 1, "tier", result.tier.as_str(), true);
    let (disposition, detail) = match result.engine_compat {
        EngineCompatDisposition::NativeAccelerated { native_subsystem } => {
            ("native_accelerated", Some(("native_subsystem", native_subsystem)))
        }
        EngineCompatDisposition::NativeGameplayPrimitive { engine } => {
            ("native_gameplay_primitive", Some(("engine", engine)))
        }
        EngineCompatDisposition::CoexistResourceBridge { bridge } => {
            ("coexist_resource_bridge", Some(("bridge", bridge)))
        }
        EngineCompatDisposition::StandardModAdapter => ("standard_mod_adapter", None),
    };
    let _ = writeln!(out, "  \"engine_compat\": {{");
    jf_str(&mut out, 2, "disposition", disposition, detail.is_some());
    if let Some((key, value)) = detail {
        jf_str(&mut out, 2, key, value, false);
    }
    let _ = writeln!(out, "  }},");

    let _ = writeln!(out, "  \"route\": {{");
    match &result.route {
        ModRouteDecision::SecurityQuarantine { .. } => {
            jf_str(&mut out, 2, "decision", "security_quarantine", false);
        }
        ModRouteDecision::RouteToNativeSubsystem {
            subsystem,
            matched_by,
            evidence,
        } => {
            jf_str(&mut out, 2, "decision", "route_to_native_subsystem", true);
            jf_str(&mut out, 2, "subsystem", subsystem.as_str(), true);
            jf_str(&mut out, 2, "matched_by", matched_by.as_str(), true);
            jf_str_list(&mut out, 2, "evidence", evidence, false);
        }
        ModRouteDecision::HybridSplit {
            client_render_fallback,
            evidence,
        } => {
            jf_str(&mut out, 2, "decision", "hybrid_split", true);
            jf_str(
                &mut out,
                2,
                "client_render_fallback",
                client_render_fallback.as_str(),
                true,
            );
            jf_str_list(&mut out, 2, "evidence", evidence, false);
        }
        ModRouteDecision::StandardPipeline => {
            jf_str(&mut out, 2, "decision", "standard_pipeline", false);
        }
    }
    let _ = writeln!(out, "  }},");
    let _ = writeln!(out, "  \"security_threats\": [");
    for (index, threat) in result.security_threats.iter().enumerate() {
        let _ = writeln!(out, "    {{");
        jf_str(&mut out, 3, "kind", threat.kind.as_str(), true);
        jf_str(&mut out, 3, "member", &threat.member, true);
        jf_str(&mut out, 3, "indicator", &threat.indicator, false);
        let _ = writeln!(
            out,
            "    }}{}",
            if index + 1 == result.security_threats.len() {
                ""
            } else {
                ","
            }
        );
    }
    let _ = writeln!(out, "  ],");

    match &result.descriptor {
        None => {
            jf_opt(&mut out, 1, "version", None, true);
            jf_opt(&mut out, 1, "loader", None, true);
            jf_opt(&mut out, 1, "environment", None, true);
            let _ = writeln!(out, "  \"dependencies\": [],");
        }
        Some(d) => {
            jf_opt(&mut out, 1, "version", Some(d.version.as_str()), true);
            jf_str(&mut out, 1, "loader", d.loader.as_str(), true);
            jf_str(&mut out, 1, "environment", env_str(d.environment), true);
            if d.dependencies.is_empty() {
                let _ = writeln!(out, "  \"dependencies\": [],");
            } else {
                let _ = writeln!(out, "  \"dependencies\": [");
                for (i, dep) in d.dependencies.iter().enumerate() {
                    let last = i + 1 == d.dependencies.len();
                    let _ = writeln!(out, "    {{");
                    jf_str(&mut out, 3, "mod_id", &dep.mod_id, true);
                    jf_opt(
                        &mut out,
                        3,
                        "version_range",
                        dep.version_range.as_deref(),
                        true,
                    );
                    jf_bool(&mut out, 3, "required", dep.required, false);
                    let _ = writeln!(out, "    }}{}", if last { "" } else { "," });
                }
                let _ = writeln!(out, "  ],");
            }
        }
    }

    json_data_summary(&mut out, 1, &result.data);
    json_data_bundle(&mut out, 1, &result.data_bundle);

    match &result.mixin_scan {
        None => {
            let _ = writeln!(out, "  \"mixin_scan\": null,");
        }
        Some(scan) => {
            json_mixin_scan(&mut out, 1, scan, true);
        }
    }

    match &result.culinary {
        None => {
            let _ = writeln!(out, "  \"culinary\": null,");
        }
        Some(c) => {
            let _ = writeln!(out, "  \"culinary\": {{");
            jf_u64(&mut out, 2, "jar_members", c.jar_members as u64, true);
            jf_u64(&mut out, 2, "data_files", c.data_files as u64, true);
            jf_u64(&mut out, 2, "cooking_recipes", c.cooking_recipes as u64, true);
            jf_u64(&mut out, 2, "cutting_recipes", c.cutting_recipes as u64, true);
            jf_u64(&mut out, 2, "skipped_recipes", c.skipped_recipes as u64, true);
            jf_u64(&mut out, 2, "tag_files", c.tag_files as u64, true);
            jf_u64(&mut out, 2, "tag_values", c.tag_values as u64, true);
            jf_u64(&mut out, 2, "loot_modifiers", c.loot_modifiers as u64, true);
            jf_u64(&mut out, 2, "food_specs", c.food_specs as u64, false);
            let _ = writeln!(out, "  }},");
        }
    }

    match &result.dimensions {
        None => {
            let _ = writeln!(out, "  \"dimensions\": null,");
        }
        Some(d) => {
            let custom: Vec<String> = match d.classification() {
                DimensionModClassification::CustomChunkGenerator(kinds) => kinds,
                DimensionModClassification::PureDataDriven => Vec::new(),
            };
            let _ = writeln!(out, "  \"dimensions\": {{");
            jf_str_list(
                &mut out,
                2,
                "dimension_types",
                &d.dimension_types
                    .iter()
                    .map(|t| t.id.clone())
                    .collect::<Vec<_>>(),
                true,
            );
            jf_str_list(
                &mut out,
                2,
                "dimensions",
                &d.dimensions
                    .iter()
                    .map(|t| t.id.clone())
                    .collect::<Vec<_>>(),
                true,
            );
            jf_str_list(&mut out, 2, "custom_chunk_generators", &custom, true);
            jf_u64(&mut out, 2, "ore_blocks", d.ore_blocks.len() as u64, true);
            jf_u64(&mut out, 2, "chest_blocks", d.chest_blocks.len() as u64, true);
            jf_str_list(&mut out, 2, "skipped_malformed", &d.skipped_malformed, false);
            let _ = writeln!(out, "  }},");
        }
    }

    jf_str_list(&mut out, 1, "audit_notes", &result.audit_notes, true);
    jf_str_list(&mut out, 1, "blockers", &result.blockers, true);
    jf_opt(
        &mut out,
        1,
        "bundle_file",
        result
            .bundle
            .as_ref()
            .and_then(|b| bundle_file_name(&b.mod_id).ok())
            .as_deref(),
        true,
    );
    match &result.bundle {
        None => {
            let _ = writeln!(out, "  \"bundle\": null");
        }
        Some(bundle) => {
            // The deterministic bundle document splices in verbatim — it
            // is itself valid JSON.
            let _ = writeln!(out, "  \"bundle\": {}", bundle.to_json());
        }
    }
    out.push_str("}\n");
    out
}

fn mods_dir_report_json(report: &ModsDirAdaptationReport) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(1024 + report.jars.len() * 256);
    out.push_str("{\n");
    jf_str(
        &mut out,
        1,
        "schema",
        "mc-spine-mods-dir-adaptation/1",
        true,
    );
    jf_str(&mut out, 1, "dir", &report.dir, true);
    jf_u64(&mut out, 1, "jars_found", report.jars.len() as u64, true);
    jf_u64(
        &mut out,
        1,
        "non_jar_entries",
        report.non_jar_entries as u64,
        true,
    );
    let _ = writeln!(out, "  \"totals\": {{");
    for (i, tier) in [
        AdaptationTier::AutoSkipped,
        AdaptationTier::NativeBundle,
        AdaptationTier::Tier2LiftedShim,
        AdaptationTier::Tier3FallbackRequired,
    ]
    .iter()
    .enumerate()
    {
        jf_u64(
            &mut out,
            2,
            tier.as_str(),
            report.tier_count(*tier) as u64,
            i < 3,
        );
    }
    let _ = writeln!(out, "  }},");
    jf_str(
        &mut out,
        1,
        "verdict",
        if report.requires_fallback() {
            "requires_fallback"
        } else {
            "adapted"
        },
        true,
    );
    jf_str_list(&mut out, 1, "audit_notes", &report.audit_notes, true);

    if report.jars.is_empty() {
        let _ = writeln!(out, "  \"jars\": []");
    } else {
        let _ = writeln!(out, "  \"jars\": [");
        for (i, jar) in report.jars.iter().enumerate() {
            let last = i + 1 == report.jars.len();
            // Splice the per-JAR document (itself valid JSON) re-indented
            // one level so the report stays strictly nested.
            let doc = jar.to_json();
            let indented: Vec<&str> = doc.trim_end().split('\n').collect();
            for (li, line) in indented.iter().enumerate() {
                if li + 1 == indented.len() {
                    let _ = writeln!(out, "    {line}{}", if last { "" } else { "," });
                } else {
                    let _ = writeln!(out, "    {line}");
                }
            }
        }
        let _ = writeln!(out, "  ]");
    }
    out.push_str("}\n");
    out
}

// ---------------------------------------------------------------------------
// Human-facing formatters
// ---------------------------------------------------------------------------

fn format_adapt_human(result: &JarAdaptationResult) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let id = result.mod_id().unwrap_or("<unknown id>");
    let _ = writeln!(
        out,
        "Adaptation of \"{}\" [{}]: {}",
        result.jar,
        id,
        result.tier.as_str()
    );
    let d = &result.data;
    let _ = writeln!(
        out,
        "  data: {} namespace(s), {} recipe file(s), {} tag file(s), {} dimension file(s), {} loot table file(s), {} class member(s)",
        d.namespaces.len(),
        d.recipe_files,
        d.tag_files,
        d.dimension_files + d.dimension_type_files,
        d.loot_table_files,
        d.java_class_members,
    );
    let db = &result.data_bundle;
    let extracted = db.recipes.len()
        + db.tags.len()
        + db.loot_tables.len()
        + db.dimensions.dimensions.len()
        + db.dimensions.dimension_types.len()
        + db.data_maps.keys().len()
        + db.biome_modifiers.len();
    if extracted > 0 || !db.rejected_members.is_empty() {
        let _ = writeln!(
            out,
            "  extracted: {} recipe(s), {} tag(s), {} loot table(s), {} dimension document(s), {} data map(s), {} biome modifier(s), {} rejected member(s)",
            db.recipes.len(),
            db.tags.len(),
            db.loot_tables.len(),
            db.dimensions.dimensions.len() + db.dimensions.dimension_types.len(),
            db.data_maps.keys().len(),
            db.biome_modifiers.len(),
            db.rejected_members.len(),
        );
    }
    match &result.route {
        ModRouteDecision::SecurityQuarantine { .. } => {
            let _ = writeln!(
                out,
                "  route: SECURITY_QUARANTINE (execution and JVM delegation prohibited)"
            );
        }
        ModRouteDecision::RouteToNativeSubsystem {
            subsystem,
            matched_by,
            evidence,
        } => {
            let _ = writeln!(
                out,
                "  route: {} via {} ({})",
                subsystem.as_str(),
                matched_by.as_str(),
                evidence.join(", ")
            );
        }
        ModRouteDecision::HybridSplit {
            client_render_fallback,
            evidence,
        } => {
            let _ = writeln!(
                out,
                "  route: hybrid_split client {} ({})",
                client_render_fallback.as_str(),
                evidence.join(", ")
            );
        }
        ModRouteDecision::StandardPipeline => {
            let _ = writeln!(out, "  route: standard_pipeline");
        }
    }
    for threat in &result.security_threats {
        let _ = writeln!(
            out,
            "  SECURITY_QUARANTINE: {} in {} ({})",
            threat.kind.as_str(),
            threat.member,
            threat.indicator
        );
    }
    if let Some(c) = &result.culinary {
        if c.cooking_recipes + c.cutting_recipes + c.loot_modifiers + c.food_specs > 0 {
            let _ = writeln!(
                out,
                "  culinary: {} cooking, {} cutting, {} loot modifier(s), {} food spec(s)",
                c.cooking_recipes, c.cutting_recipes, c.loot_modifiers, c.food_specs,
            );
        }
    }
    if let Some(scan) = &result.mixin_scan {
        let _ = writeln!(
            out,
            "  mixins: {} config(s), {} site(s), {}",
            scan.mixin_configs_found,
            scan.sites.len(),
            match &scan.verdict {
                MixinLiftVerdict::FullyLiftable(b) =>
                    format!("{} lifted", b.len()),
                MixinLiftVerdict::RequiresFallback { unliftable, .. } =>
                    format!("{} unliftable", unliftable.len()),
            },
        );
    }
    for note in &result.audit_notes {
        let _ = writeln!(out, "  - {note}");
    }
    for blocker in &result.blockers {
        let _ = writeln!(out, "  ! {blocker}");
    }
    match result.tier {
        AdaptationTier::AutoSkipped => {
            let _ = write!(out, "Result: auto-skipped — {REASON_SUPERSEDED}.");
        }
        AdaptationTier::NativeBundle => {
            let _ = write!(
                out,
                "Result: native — a validated {BUNDLE_SCHEMA} bundle was produced for '{id}'."
            );
        }
        AdaptationTier::Tier2LiftedShim => {
            let _ = write!(
                out,
                "Result: Tier 2 — every mixin site lifted onto standard event hooks (bounded JVM shim)."
            );
        }
        AdaptationTier::Tier3FallbackRequired => {
            if matches!(result.route, ModRouteDecision::SecurityQuarantine { .. }) {
                let _ = write!(
                    out,
                    "Result: SECURITY_QUARANTINE — this JAR must never execute or be delegated."
                );
            } else {
                let _ = write!(out, "Result: Tier 3 — {} blocker(s) require a fallback (quarantine or upstream loader).", result.blockers.len());
            }
        }
    }
    out
}

fn format_adapt_dir_human(report: &ModsDirAdaptationReport) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Adaptation of mods directory \"{}\"",
        report.dir
    );
    let _ = writeln!(
        out,
        "Found {} JAR(s){}; {} auto-skipped.",
        report.jars.len(),
        if report.non_jar_entries > 0 {
            format!(
                " ({} non-JAR entr{} ignored)",
                report.non_jar_entries,
                if report.non_jar_entries == 1 { "y" } else { "ies" }
            )
        } else {
            String::new()
        },
        report.tier_count(AdaptationTier::AutoSkipped),
    );
    for jar in &report.jars {
        let id = jar.mod_id().unwrap_or("<unknown id>");
        let detail = match jar.tier {
            AdaptationTier::AutoSkipped => {
                format!("auto-skipped — {REASON_SUPERSEDED}")
            }
            AdaptationTier::NativeBundle => {
                "native — mc-spine-mod-bundle/1 produced".to_string()
            }
            AdaptationTier::Tier2LiftedShim => {
                let lifted = jar
                    .mixin_scan
                    .as_ref()
                    .map(|s| lifted_bindings(&s.verdict).len())
                    .unwrap_or(0);
                format!("Tier 2 — {lifted} mixin site(s) lifted")
            }
            AdaptationTier::Tier3FallbackRequired => {
                format!("Tier 3 — {}", jar.blockers.join("; "))
            }
        };
        let _ = write!(out, "  {} [{id}]: {detail}", jar.jar);
        if let Some(note) = jar
            .audit_notes
            .iter()
            .find(|note| note.starts_with("engine compatibility strategy:"))
        {
            let _ = write!(out, "; {note}");
        }
        let _ = writeln!(out);
    }
    for note in &report.audit_notes {
        let _ = writeln!(out, "  - {note}");
    }
    let _ = writeln!(
        out,
        "Totals: {} auto-skipped, {} native bundle, {} Tier 2 shim, {} Tier 3 fallback.",
        report.tier_count(AdaptationTier::AutoSkipped),
        report.tier_count(AdaptationTier::NativeBundle),
        report.tier_count(AdaptationTier::Tier2LiftedShim),
        report.tier_count(AdaptationTier::Tier3FallbackRequired),
    );
    if report.requires_fallback() {
        let _ = write!(
            out,
            "Result: requires fallback — {} JAR(s) stay at Tier 3 (quarantine or upstream loader).",
            report.tier_count(AdaptationTier::Tier3FallbackRequired)
        );
    } else {
        let _ = write!(
            out,
            "Result: adapted — every JAR runs natively or on a bounded Tier 2 shim; no fallback needed."
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // -------------------------------------------------------------------
    // Stored-ZIP JAR builder
    // -------------------------------------------------------------------

    fn crc32(data: &[u8]) -> u32 {
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

    /// Pack `(name, bytes)` members into a real stored-method ZIP archive.
    fn jar(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, data) in entries {
            let name = name.as_bytes();
            let crc = crc32(data);
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
            central.extend(0u16.to_le_bytes());
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

    // -------------------------------------------------------------------
    // Minimal .class builder (@Mixin class + annotated ()V methods)
    // -------------------------------------------------------------------

    enum CpItem {
        Utf8(String),
        Class(u16),
        Int(i32),
    }

    #[derive(Default)]
    struct Cp(Vec<CpItem>);

    impl Cp {
        fn push(&mut self, item: CpItem) -> u16 {
            self.0.push(item);
            self.0.len() as u16
        }
        fn utf8(&mut self, s: &str) -> u16 {
            self.push(CpItem::Utf8(s.to_string()))
        }
        fn class(&mut self, internal_name: &str) -> u16 {
            let name_index = self.utf8(internal_name);
            self.push(CpItem::Class(name_index))
        }
        fn int(&mut self, v: i32) -> u16 {
            self.push(CpItem::Int(v))
        }
        fn encode(&self) -> Vec<u8> {
            let mut out = Vec::new();
            for item in &self.0 {
                match item {
                    CpItem::Utf8(s) => {
                        out.push(1);
                        out.extend((s.len() as u16).to_be_bytes());
                        out.extend(s.as_bytes());
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

    enum Ev {
        S(String),
        Z(bool),
        Nested(&'static str, Vec<(&'static str, Ev)>),
        Arr(Vec<Ev>),
    }

    fn ev_s(v: &str) -> Ev {
        Ev::S(v.to_string())
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
            Ev::Nested(desc, elems) => {
                out.push(b'@');
                write_ann(out, cp, desc, elems);
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

    fn write_ann(out: &mut Vec<u8>, cp: &mut Cp, desc: &str, elems: &[(&str, Ev)]) {
        out.extend(cp.utf8(desc).to_be_bytes());
        out.extend((elems.len() as u16).to_be_bytes());
        for (name, ev) in elems {
            out.extend(cp.utf8(name).to_be_bytes());
            write_ev(out, cp, ev);
        }
    }

    const MIXIN_DESC: &str = "Lorg/spongepowered/asm/mixin/Mixin;";
    const INJECT_DESC: &str = "Lorg/spongepowered/asm/mixin/injection/Inject;";
    const AT_DESC: &str = "Lorg/spongepowered/asm/mixin/injection/At;";
    const REDIRECT_DESC: &str = "Lorg/spongepowered/asm/mixin/injection/Redirect;";

    fn at(value: &str) -> Ev {
        Ev::Nested(AT_DESC, vec![("value", ev_s(value))])
    }

    struct FixtureMethod {
        name: &'static str,
        ann_desc: &'static str,
        elems: Vec<(&'static str, Ev)>,
    }

    /// Minimal structurally-valid `.class`: public class `internal`
    /// extending `java/lang/Object`, each method `()V` carrying a single
    /// `RuntimeInvisibleAnnotations` annotation, plus a class-level
    /// `@Mixin(targets=[...])`.
    fn build_mixin_class(
        internal: &str,
        targets: &[&str],
        methods: &[FixtureMethod],
    ) -> Vec<u8> {
        let mut cp = Cp::default();
        let this_idx = cp.class(internal);
        let super_idx = cp.class("java/lang/Object");
        let rva_idx = cp.utf8("RuntimeInvisibleAnnotations");
        let desc_idx = cp.utf8("()V");

        let mut mixin_payload = 1u16.to_be_bytes().to_vec();
        write_ann(
            &mut mixin_payload,
            &mut cp,
            MIXIN_DESC,
            &[(
                "targets",
                Ev::Arr(targets.iter().map(|t| ev_s(t)).collect()),
            )],
        );

        let mut method_parts = Vec::new();
        for m in methods {
            let name_idx = cp.utf8(m.name);
            let mut payload = 1u16.to_be_bytes().to_vec();
            write_ann(&mut payload, &mut cp, m.ann_desc, &m.elems);
            method_parts.push((name_idx, payload));
        }

        let mut out = Vec::new();
        out.extend(0xCAFE_BABEu32.to_be_bytes());
        out.extend(0u16.to_be_bytes());
        out.extend(61u16.to_be_bytes());
        out.extend(((cp.0.len() + 1) as u16).to_be_bytes());
        out.extend(cp.encode());
        out.extend(0x0021u16.to_be_bytes());
        out.extend(this_idx.to_be_bytes());
        out.extend(super_idx.to_be_bytes());
        out.extend(0u16.to_be_bytes()); // interfaces
        out.extend(0u16.to_be_bytes()); // fields
        out.extend((methods.len() as u16).to_be_bytes());
        for (name_idx, payload) in &method_parts {
            out.extend(0x0001u16.to_be_bytes());
            out.extend(name_idx.to_be_bytes());
            out.extend(desc_idx.to_be_bytes());
            out.extend(1u16.to_be_bytes());
            out.extend(rva_idx.to_be_bytes());
            out.extend((payload.len() as u32).to_be_bytes());
            out.extend(payload);
        }
        out.extend(1u16.to_be_bytes());
        out.extend(rva_idx.to_be_bytes());
        out.extend((mixin_payload.len() as u32).to_be_bytes());
        out.extend(mixin_payload);
        out
    }

    /// A plain `.class` with no annotations at all — real bytecode with an
    /// empty mixin surface.
    fn build_plain_class(internal: &str) -> Vec<u8> {
        let mut cp = Cp::default();
        let this_idx = cp.class(internal);
        let super_idx = cp.class("java/lang/Object");
        let desc_idx = cp.utf8("()V");
        let name_idx = cp.utf8("init");
        let mut out = Vec::new();
        out.extend(0xCAFE_BABEu32.to_be_bytes());
        out.extend(0u16.to_be_bytes());
        out.extend(61u16.to_be_bytes());
        out.extend(((cp.0.len() + 1) as u16).to_be_bytes());
        out.extend(cp.encode());
        out.extend(0x0021u16.to_be_bytes());
        out.extend(this_idx.to_be_bytes());
        out.extend(super_idx.to_be_bytes());
        out.extend(0u16.to_be_bytes()); // interfaces
        out.extend(0u16.to_be_bytes()); // fields
        out.extend(1u16.to_be_bytes()); // one method, no attributes
        out.extend(0x0001u16.to_be_bytes());
        out.extend(name_idx.to_be_bytes());
        out.extend(desc_idx.to_be_bytes());
        out.extend(0u16.to_be_bytes());
        out.extend(0u16.to_be_bytes()); // no class attributes
        out
    }

    // -------------------------------------------------------------------
    // Fixture JARs
    // -------------------------------------------------------------------

    fn fabric_manifest(id: &str, version: &str, extra: &str) -> String {
        format!(
            r#"{{"schemaVersion":1,"id":"{id}","version":"{version}","environment":"*"{extra}}}"#
        )
    }

    fn opts() -> AutoAdaptOptions {
        AutoAdaptOptions::default()
    }

    /// `sodium-*.jar`: filename-inferred superseded id.
    fn superseded_by_name_jar() -> Vec<u8> {
        let manifest = fabric_manifest("sodium", "0.6.13", "");
        jar(&[
            ("fabric.mod.json", manifest.as_bytes()),
            ("com/sodium/Thing.class", build_plain_class("com/sodium/Thing").as_slice()),
        ])
    }

    /// `coolmod-1.0.jar` whose manifest declares the superseded `lithium`.
    fn superseded_by_manifest_jar() -> Vec<u8> {
        let manifest = fabric_manifest("lithium", "0.15.0", "");
        jar(&[
            ("fabric.mod.json", manifest.as_bytes()),
            ("com/lithium/X.class", build_plain_class("com/lithium/X").as_slice()),
        ])
    }

    /// Pure datapack JAR: `pack.mcmeta` + generic recipe/tag data.
    fn datapack_jar() -> Vec<u8> {
        jar(&[
            ("pack.mcmeta", br#"{"pack":{"pack_format":48,"description":"dp"}}"#),
            (
                "data/tastypack/recipes/soup.json",
                br#"{"type":"minecraft:crafting_shapeless","ingredients":["minecraft:bowl"],"result":{"id":"minecraft:beetroot_soup","count":1}}"#,
            ),
            (
                "data/tastypack/tags/items/bowls.json",
                br#"{"values":["minecraft:bowl"]}"#,
            ),
        ])
    }

    /// Culinary mod: Fabric manifest + a `*:cooking` recipe + an item tag.
    fn culinary_jar() -> Vec<u8> {
        let manifest = fabric_manifest("tastymod", "1.2.3", "");
        jar(&[
            ("fabric.mod.json", manifest.as_bytes()),
            ("com/tastymod/Entry.class", build_plain_class("com/tastymod/Entry").as_slice()),
            (
                "data/tastymod/recipes/stew.json",
                br#"{"type":"tastymod:cooking","ingredients":["minecraft:potato","minecraft:carrot"],"result":{"id":"tastymod:stew","count":1},"cookingtime":200}"#,
            ),
            (
                "data/tastymod/tags/items/veggies.json",
                br#"{"values":["minecraft:potato","minecraft:carrot"]}"#,
            ),
        ])
    }

    /// Liftable mixin mod: `@Inject(method="hurt", at=@At("HEAD"))` on
    /// `LivingEntity` — binds `EntityDamagePre`.
    fn liftable_mixin_jar() -> Vec<u8> {
        let manifest = fabric_manifest("safemod", "1.0.0", r#","mixins":["safe.mixins.json"]"#);
        let config = br#"{
            "package": "com.safemod.mixin",
            "mixins": ["LivingEntityMixin"]
        }"#;
        let class = build_mixin_class(
            "com/safemod/mixin/LivingEntityMixin",
            &["net.minecraft.world.entity.LivingEntity"],
            &[FixtureMethod {
                name: "safemod$onHurt",
                ann_desc: INJECT_DESC,
                elems: vec![
                    ("method", ev_s("hurt")),
                    ("at", at("HEAD")),
                    ("cancellable", Ev::Z(false)),
                ],
            }],
        );
        jar(&[
            ("fabric.mod.json", manifest.as_bytes()),
            ("safe.mixins.json", config),
            (
                "com/safemod/mixin/LivingEntityMixin.class",
                class.as_slice(),
            ),
        ])
    }

    /// Unliftable mixin mod: a `@Redirect` site stays at Tier 3.
    fn unliftable_mixin_jar() -> Vec<u8> {
        let manifest = fabric_manifest("badmod", "1.0.0", r#","mixins":["bad.mixins.json"]"#);
        let config = br#"{
            "package": "com.badmod.mixin",
            "mixins": ["BrokenMixin"]
        }"#;
        let class = build_mixin_class(
            "com/badmod/mixin/BrokenMixin",
            &["net.minecraft.world.entity.LivingEntity"],
            &[FixtureMethod {
                name: "badmod$redirectHurt",
                ann_desc: REDIRECT_DESC,
                elems: vec![
                    ("method", ev_s("hurt")),
                    (
                        "at",
                        Ev::Nested(
                            AT_DESC,
                            vec![
                                ("value", ev_s("INVOKE")),
                                ("target", ev_s("net/minecraft/x/damage")),
                            ],
                        ),
                    ),
                ],
            }],
        );
        jar(&[
            ("fabric.mod.json", manifest.as_bytes()),
            ("bad.mixins.json", config),
            ("com/badmod/mixin/BrokenMixin.class", class.as_slice()),
        ])
    }

    /// Loader JAR with Java bytecode but no mixins, no culinary, no
    /// dimension data — nothing a zero-JVM runtime can execute.
    fn bytecode_only_jar() -> Vec<u8> {
        let manifest = fabric_manifest("javamod", "9.9", "");
        jar(&[
            ("fabric.mod.json", manifest.as_bytes()),
            ("com/javamod/Main.class", build_plain_class("com/javamod/Main").as_slice()),
            ("com/javamod/Util.class", build_plain_class("com/javamod/Util").as_slice()),
        ])
    }

    /// Pure data-driven dimension mod.
    fn dimension_jar(generator_type: &str) -> Vec<u8> {
        let manifest = fabric_manifest("dimmod", "1.0", "");
        let dim_type = br#"{"min_y":0,"height":256,"has_skylight":true}"#;
        let dim = format!(
            r#"{{"type":"dimmod:mydim","generator":{{"type":"{generator_type}"}}}}"#
        );
        jar(&[
            ("fabric.mod.json", manifest.as_bytes()),
            ("data/dimmod/dimension_type/mydim.json", dim_type),
            ("data/dimmod/dimension/mydim.json", dim.as_bytes()),
        ])
    }

    fn client_only_jar() -> Vec<u8> {
        let manifest =
            br#"{"schemaVersion":1,"id":"clientmod","version":"1.0","environment":"client"}"#;
        jar(&[
            ("fabric.mod.json", manifest.as_slice()),
            ("pack.mcmeta", br#"{"pack":{"pack_format":48,"description":"c"}}"#),
        ])
    }

    fn version_mismatch_jar() -> Vec<u8> {
        let manifest = fabric_manifest(
            "oldmod",
            "1.0",
            r#","depends":{"minecraft":"1.20.1"}"#,
        );
        jar(&[
            ("fabric.mod.json", manifest.as_bytes()),
            ("data/oldmod/recipes/x.json", br#"{"type":"minecraft:crafting_shapeless","ingredients":["minecraft:stick"],"result":{"id":"minecraft:torch","count":4}}"#),
        ])
    }

    fn embedded_bundle_jar(bundle_json: &str) -> Vec<u8> {
        jar(&[
            (EMBEDDED_BUNDLE_MEMBER, bundle_json.as_bytes()),
            ("data/embmod/recipes/r.json", br#"{"type":"minecraft:crafting_shapeless","ingredients":["minecraft:stick"],"result":{"id":"minecraft:torch","count":4}}"#),
        ])
    }

    fn scaffold_doc_for(mod_id: &str) -> String {
        // A strict-valid bundle document used as the embedded fixture.
        format!(
            "{{\"schema\":\"{}\",\"mod_id\":\"{}\",\"mc_version_range\":\"[1.21,)\",\"portals\":[],\"progression_barriers\":[],\"biome_regions\":[],\"creatures\":[],\"custom_channels\":[]}}",
            BUNDLE_SCHEMA, mod_id
        )
    }

    static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mc-spine-auto-adapter-{}-{}-{tag}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("create tmp dir");
        dir
    }

    // -------------------------------------------------------------------
    // Tier tests
    // -------------------------------------------------------------------

    #[test]
    fn superseded_perf_mod_skips_before_bytecode() {
        // Filename-inferred id: no mixin_scan/culinary work is done.
        let result = auto_adapt_jar_bytes(&superseded_by_name_jar(), "sodium-0.6.13.jar", &opts())
            .expect("superseded JAR adapts");
        assert_eq!(result.tier, AdaptationTier::AutoSkipped);
        assert_eq!(result.inferred_mod_id.as_deref(), Some("sodium"));
        assert!(result.mixin_scan.is_none(), "skipped before bytecode read");
        assert!(result
            .audit_notes
            .iter()
            .any(|n| n.contains("BUILTIN_SUPERSEDED_PERF_MODS")));
    }

    #[test]
    fn declared_superseded_id_skips_even_when_renamed() {
        let result = auto_adapt_jar_bytes(
            &superseded_by_manifest_jar(),
            "totally-not-lithium.jar",
            &opts(),
        )
        .expect("renamed superseded JAR adapts");
        assert_eq!(result.tier, AdaptationTier::AutoSkipped);
        assert_eq!(result.mod_id(), Some("lithium"));
        assert!(result.mixin_scan.is_none());
    }

    #[test]
    fn pure_datapack_adapts_to_native_bundle() {
        let result =
            auto_adapt_jar_bytes(&datapack_jar(), "tastypack-1.0.jar", &opts()).unwrap();
        assert_eq!(result.tier, AdaptationTier::NativeBundle, "{:?}", result.blockers);
        let bundle = result.bundle.expect("native jar emits a bundle");
        assert_eq!(bundle.mod_id, "tastypack");
        DeclarativeModBundle::from_json(&bundle.to_json()).expect("bundle validates");
        assert_eq!(result.data.recipe_files, 1);
        assert_eq!(result.data.tag_files, 1);
        assert_eq!(result.data.java_class_members, 0);
    }

    #[test]
    fn culinary_mod_adapts_native_with_stats() {
        let result = auto_adapt_jar_bytes(&culinary_jar(), "tastymod-1.2.3.jar", &opts()).unwrap();
        assert_eq!(result.tier, AdaptationTier::NativeBundle, "{:?}", result.blockers);
        let c = result.culinary.unwrap();
        assert_eq!(c.cooking_recipes, 1);
        assert_eq!(c.tag_files, 1);
        assert!(result.data.java_class_members > 0);
        assert!(result.bundle.is_some());
    }

    #[test]
    fn liftable_mixin_jar_gets_tier2_shim() {
        let result =
            auto_adapt_jar_bytes(&liftable_mixin_jar(), "safemod-1.0.jar", &opts()).unwrap();
        assert_eq!(result.tier, AdaptationTier::Tier2LiftedShim, "{:?}", result.blockers);
        let scan = result.mixin_scan.unwrap();
        assert_eq!(scan.sites.len(), 1);
        let MixinLiftVerdict::FullyLiftable(bindings) = &scan.verdict else {
            panic!("expected fully liftable: {:?}", scan.verdict);
        };
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].hook.as_str(), "EntityDamagePre");
        assert!(result.bundle.is_none());
    }

    #[test]
    fn unliftable_mixin_jar_gets_tier3_with_named_blocker() {
        let result =
            auto_adapt_jar_bytes(&unliftable_mixin_jar(), "badmod-1.0.jar", &opts()).unwrap();
        assert_eq!(result.tier, AdaptationTier::Tier3FallbackRequired);
        assert!(result.requires_fallback());
        assert!(
            result
                .blockers
                .iter()
                .any(|b| b.contains("BrokenMixin") && b.contains("UnsupportedAnnotation")),
            "blockers must name the site and reason: {:?}",
            result.blockers
        );
        assert!(result.bundle.is_none());
    }

    #[test]
    fn bytecode_only_mod_gets_tier3() {
        let result =
            auto_adapt_jar_bytes(&bytecode_only_jar(), "javamod-9.9.jar", &opts()).unwrap();
        assert_eq!(result.tier, AdaptationTier::Tier3FallbackRequired);
        assert!(result
            .blockers
            .iter()
            .any(|b| b.contains("Java bytecode")), "{:?}", result.blockers);
    }

    #[test]
    fn data_driven_dimension_mod_is_native() {
        let result = auto_adapt_jar_bytes(
            &dimension_jar("minecraft:noise"),
            "dimmod-1.0.jar",
            &opts(),
        )
        .unwrap();
        assert_eq!(result.tier, AdaptationTier::NativeBundle, "{:?}", result.blockers);
        let dims = result.dimensions.unwrap();
        assert!(dims.is_dimension_mod());
        assert!(dims.is_pure_data_driven());
    }

    #[test]
    fn custom_chunk_generator_is_tier3() {
        let result = auto_adapt_jar_bytes(
            &dimension_jar("dimmod:quantum_generator"),
            "dimmod-1.0.jar",
            &opts(),
        )
        .unwrap();
        assert_eq!(result.tier, AdaptationTier::Tier3FallbackRequired);
        assert!(result
            .blockers
            .iter()
            .any(|b| b.contains("custom Java chunk generator")
                && b.contains("dimmod:quantum_generator")));
    }

    #[test]
    fn client_only_mod_is_auto_skipped() {
        let result = auto_adapt_jar_bytes(&client_only_jar(), "clientmod-1.0.jar", &opts())
            .unwrap();
        assert_eq!(result.tier, AdaptationTier::AutoSkipped);
        assert!(result
            .audit_notes
            .iter()
            .any(|n| n.contains("client-only")));
    }

    #[test]
    fn version_mismatch_is_tier3() {
        let result =
            auto_adapt_jar_bytes(&version_mismatch_jar(), "oldmod-1.0.jar", &opts()).unwrap();
        assert_eq!(result.tier, AdaptationTier::Tier3FallbackRequired);
        assert!(result
            .blockers
            .iter()
            .any(|b| b.contains("1.20.1") && b.contains("1.21.1")));
    }

    #[test]
    fn embedded_bundle_jar_installs_verbatim() {
        let bytes = embedded_bundle_jar(&scaffold_doc_for("embmod"));
        let result = auto_adapt_jar_bytes(&bytes, "embmod-1.0.jar", &opts()).unwrap();
        assert_eq!(result.tier, AdaptationTier::NativeBundle, "{:?}", result.blockers);
        assert_eq!(result.bundle.unwrap().mod_id, "embmod");
    }

    #[test]
    fn invalid_embedded_bundle_is_tier3() {
        let bad = r#"{"schema":"mc-spine-mod-bundle/1","mod_id":"embmod","surprise":1}"#;
        let result = auto_adapt_jar_bytes(&embedded_bundle_jar(bad), "embmod-1.0.jar", &opts())
            .unwrap();
        assert_eq!(result.tier, AdaptationTier::Tier3FallbackRequired);
        assert!(result
            .blockers
            .iter()
            .any(|b| b.contains("validation")), "{:?}", result.blockers);
    }

    #[test]
    fn mods_dir_report_covers_every_jar_class() {
        let dir = tmp_dir("mods-dir");
        fs::write(dir.join("sodium-0.6.13.jar"), superseded_by_name_jar()).unwrap();
        fs::write(dir.join("tastymod-1.2.3.jar"), culinary_jar()).unwrap();
        fs::write(dir.join("safemod-1.0.jar"), liftable_mixin_jar()).unwrap();
        fs::write(dir.join("badmod-1.0.jar"), unliftable_mixin_jar()).unwrap();
        fs::write(dir.join("notes.txt"), b"not a jar").unwrap();

        let report = auto_adapt_mods_dir(&dir, &opts()).unwrap();
        assert_eq!(report.jars.len(), 4);
        assert_eq!(report.non_jar_entries, 1);
        assert_eq!(report.tier_count(AdaptationTier::AutoSkipped), 1);
        assert_eq!(report.tier_count(AdaptationTier::NativeBundle), 1);
        assert_eq!(report.tier_count(AdaptationTier::Tier2LiftedShim), 1);
        assert_eq!(report.tier_count(AdaptationTier::Tier3FallbackRequired), 1);
        assert!(report.requires_fallback());

        // Deterministic filename order: badmod < safemod < sodium < tastymod.
        let order: Vec<&str> = report.jars.iter().map(|j| j.jar.as_str()).collect();
        assert_eq!(
            order,
            vec![
                "badmod-1.0.jar",
                "safemod-1.0.jar",
                "sodium-0.6.13.jar",
                "tastymod-1.2.3.jar"
            ]
        );

        // The whole report must be valid, deterministic JSON.
        let json = report.to_json();
        assert_eq!(json, report.to_json());
        crate::data_loader::parse_json(json.as_bytes()).expect("report is valid JSON");
        let human = report.format_human();
        assert!(human.contains("requires fallback"));
    }

    #[test]
    fn mods_dir_duplicate_mod_ids_are_audited() {
        let dir = tmp_dir("dup-ids");
        // Two JARs, same declared mod id.
        fs::write(dir.join("tastymod-a.jar"), culinary_jar()).unwrap();
        fs::write(dir.join("tastymod-b.jar"), culinary_jar()).unwrap();
        let report = auto_adapt_mods_dir(&dir, &opts()).unwrap();
        assert!(report
            .audit_notes
            .iter()
            .any(|n| n.contains("tastymod") && n.contains("also declares")));
    }

    #[test]
    fn corrupt_jar_is_an_error_not_a_tier() {
        let result = auto_adapt_jar_bytes(b"PK\x03\x04garbage", "broken.jar", &opts());
        assert!(result.is_err());
        let dir = tmp_dir("corrupt");
        assert!(auto_adapt_mods_dir(&dir.join("missing"), &opts()).is_err());
    }

    // -------------------------------------------------------------------
    // Output writing
    // -------------------------------------------------------------------

    #[test]
    fn write_outputs_emits_bundles_and_report() {
        let dir = tmp_dir("out");
        let jar_dir = tmp_dir("out-jars");
        fs::write(jar_dir.join("tastymod-1.2.3.jar"), culinary_jar()).unwrap();
        fs::write(jar_dir.join("badmod-1.0.jar"), unliftable_mixin_jar()).unwrap();
        let report = auto_adapt_mods_dir(&jar_dir, &opts()).unwrap();

        let out_dir = dir.join("adapted");
        let written =
            write_adaptation_outputs(&out_dir, &report.to_json(), &report.jars).unwrap();
        let bundle_path = out_dir.join("tastymod.bundle.json");
        let report_path = out_dir.join(ADAPTATION_REPORT_FILE);
        assert_eq!(written, vec![bundle_path.clone(), report_path.clone()]);

        let bundle_text = fs::read_to_string(&bundle_path).unwrap();
        let parsed = DeclarativeModBundle::from_json(&bundle_text).unwrap();
        assert_eq!(parsed.mod_id, "tastymod");
        crate::data_loader::parse_json(fs::read(&report_path).unwrap().as_slice())
            .expect("report file is valid JSON");
    }

    #[test]
    fn unsafe_bundle_ids_are_rejected() {
        for bad in ["", "..", "../evil", "a/b", "a\\b", "a:b", ".hidden", "A mod", "   "] {
            assert!(
                bundle_file_name(bad).is_err(),
                "'{bad}' must be rejected"
            );
        }
        assert_eq!(bundle_file_name("mymod").unwrap(), "mymod.bundle.json");
        assert_eq!(
            bundle_file_name("my-mod_v2.1").unwrap(),
            "my-mod_v2.1.bundle.json"
        );
    }

    // -------------------------------------------------------------------
    // scaffold_native_bundle
    // -------------------------------------------------------------------

    #[test]
    fn scaffold_emits_a_validated_bundle_document() {
        let json = scaffold_native_bundle("mymod", "My Cool Mod", "1.21.1").unwrap();
        let bundle = DeclarativeModBundle::from_json(&json).expect("scaffold validates");
        assert_eq!(bundle.mod_id, "mymod");
        assert_eq!(bundle.mc_version_range, "1.21.1");

        let json = scaffold_native_bundle("mymod2", "", "").unwrap();
        let bundle = DeclarativeModBundle::from_json(&json).unwrap();
        assert_eq!(bundle.mc_version_range, "[1.21,)");
    }

    #[test]
    fn scaffold_rejects_bad_ids_and_fields() {
        for bad in ["", "  ", "My Mod", "ns:mod", "..", "a/b", "UPPER", "mod!"] {
            assert!(
                scaffold_native_bundle(bad, "", "").is_err(),
                "'{bad}' must be rejected"
            );
        }
        assert!(scaffold_native_bundle("ok", "n\x07ame", "").is_err());
        assert!(scaffold_native_bundle("ok", "", "not a range [oops").is_err());
    }

    // -------------------------------------------------------------------
    // Report JSON
    // -------------------------------------------------------------------

    #[test]
    fn jar_result_json_is_deterministic_and_valid() {
        let result =
            auto_adapt_jar_bytes(&liftable_mixin_jar(), "safemod-1.0.jar", &opts()).unwrap();
        let json = result.to_json();
        assert_eq!(json, result.to_json());
        crate::data_loader::parse_json(json.as_bytes()).expect("result JSON parses");
        assert!(json.contains("mc-spine-jar-adaptation/1"));
        assert!(json.contains("EntityDamagePre"));
        assert!(result.format_human().contains("Tier 2"));
    }
}
