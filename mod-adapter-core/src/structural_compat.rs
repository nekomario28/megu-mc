// SPDX-License-Identifier: LGPL-3.0-or-later

//! Structural, versionless mod capability inspection and deterministic
//! three-tier safe-load verdicts.
//!
//! The inspector deliberately does **not** trust a Minecraft version number or
//! a hand-written compatibility claim by itself. It reads the mod's internal
//! structure — declarative bundle metadata, datapack JSON paths and resource
//! references, JVM class constant-pool symbols, Mixin bytecode, loader
//! extension surfaces, and WASM host imports — then compares the resulting
//! clean capability requirements with the active [`WorldProfile`].
//!
//! Public capability names stay versionless (`recipe`, not `recipe_v1`).
//! Compatible contract evolution is represented by the integer `contract_rev`
//! floor inside [`CapabilityRequirement::min_rev`]; a newer engine contract can
//! shim an older requirement, while a newer requirement on an older engine is
//! rejected before world state is touched.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use megu_core::id::ResourceLocation;
use megu_core::json::JsonValue as WireJson;

use crate::auto_adapter::{extract_mod_data, EMBEDDED_BUNDLE_MEMBER};
use crate::class_mixin_scanner::{scan_jar_mixins_with_options, JarScanOptions};
use crate::data_loader::{parse_json, JsonValue};
use crate::dimension_mod::PortalTrigger;
use crate::ecosystem_compat::scan_mod_jar_named;
use crate::loader_extensions::{inspect_jar_extensions, BiomeModifierKind};
use crate::mixin_lifter::{MixinLiftVerdict, MixinSite};
use crate::modpack::{read_zip_member, zip_member_list};
use crate::unified_bundle::{BundleCapabilityRequirement, DeclarativeModBundle};

/// Verdict JSON schema emitted by [`CompatLoadVerdict::to_json`].
pub const STRUCTURAL_COMPAT_SCHEMA_VERSION: u32 = 1;

/// Highest number of archive members whose class bytecode the structural
/// inspector scans. This mirrors the mixin scanner's defensive bound: a mod
/// beyond it is rejected rather than silently half-inspected.
const MAX_BYTECODE_MEMBERS: usize = 65_536;

/// Total decompressed bytecode bytes inspected in one archive.
const MAX_BYTECODE_BYTES: u64 = 256 * 1024 * 1024;

/// Errors produced while inspecting an artifact. Evaluation itself is
/// total: unsupported or unsafe mods become [`CompatLoadVerdict::Blocked`]
/// rather than an error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StructuralCompatError {
    /// The ZIP/JAR container or one of its members failed bounded reading.
    Archive(String),
    /// A declarative bundle or loader/native manifest was malformed.
    Manifest(String),
    /// Internal bytecode/WASM structure was malformed or exceeded bounds.
    Validation(String),
}

impl StructuralCompatError {
    pub fn reason(&self) -> &str {
        match self {
            Self::Archive(m) | Self::Manifest(m) | Self::Validation(m) => m,
        }
    }
}

impl fmt::Display for StructuralCompatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.reason())
    }
}

impl std::error::Error for StructuralCompatError {}

/// Provenance of one capability requirement. The evidence is intentionally
/// exact: a manifest field, class symbol, datapack path, bundle section, or
/// WASM import pair — never merely "the mod needs this".
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RequirementSource {
    DeclaredManifest { field: String },
    InferredBytecode { symbol: String },
    InferredDatapack { path: String },
    InferredBundle { path: String },
    InferredWasmImport { module: String, name: String },
}

impl RequirementSource {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::DeclaredManifest { .. } => "declared_manifest",
            Self::InferredBytecode { .. } => "inferred_bytecode",
            Self::InferredDatapack { .. } => "inferred_datapack",
            Self::InferredBundle { .. } => "inferred_bundle",
            Self::InferredWasmImport { .. } => "inferred_wasm_import",
        }
    }

    pub fn evidence(&self) -> String {
        match self {
            Self::DeclaredManifest { field } => field.clone(),
            Self::InferredBytecode { symbol } => symbol.clone(),
            Self::InferredDatapack { path } | Self::InferredBundle { path } => path.clone(),
            Self::InferredWasmImport { module, name } => format!("{module}:{name}"),
        }
    }
}

/// `true` when `name` is a clean lowercase snake-case capability name with
/// no embedded `_v<N>` contract-version suffix.
pub fn is_clean_capability_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 64 || !name.as_bytes()[0].is_ascii_lowercase() {
        return false;
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        return false;
    }
    match name.rsplit_once("_v") {
        Some((prefix, rev)) => {
            prefix.is_empty() || rev.is_empty() || !rev.bytes().all(|b| b.is_ascii_digit())
        }
        None => true,
    }
}

/// One versionless capability requirement. `min_rev` is the internal
/// `contract_rev` floor; it is never part of `name`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapabilityRequirement {
    pub name: String,
    pub min_rev: Option<u32>,
    pub optional: bool,
    pub source: RequirementSource,
}

impl CapabilityRequirement {
    pub fn declared(name: impl Into<String>, min_rev: Option<u32>, optional: bool) -> Self {
        Self::declared_field(name, min_rev, optional, "requires")
    }

    pub fn declared_field(
        name: impl Into<String>,
        min_rev: Option<u32>,
        optional: bool,
        field: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            min_rev,
            optional,
            source: RequirementSource::DeclaredManifest {
                field: field.into(),
            },
        }
    }

    pub fn inferred_datapack(
        name: impl Into<String>,
        min_rev: Option<u32>,
        optional: bool,
        path: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            min_rev,
            optional,
            source: RequirementSource::InferredDatapack { path: path.into() },
        }
    }

    pub fn inferred_bundle(
        name: impl Into<String>,
        min_rev: Option<u32>,
        optional: bool,
        path: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            min_rev,
            optional,
            source: RequirementSource::InferredBundle { path: path.into() },
        }
    }

    pub fn inferred_bytecode(
        name: impl Into<String>,
        min_rev: Option<u32>,
        optional: bool,
        symbol: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            min_rev,
            optional,
            source: RequirementSource::InferredBytecode {
                symbol: symbol.into(),
            },
        }
    }

    pub fn inferred_wasm_import(
        name: impl Into<String>,
        min_rev: Option<u32>,
        optional: bool,
        module: impl Into<String>,
        import: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            min_rev,
            optional,
            source: RequirementSource::InferredWasmImport {
                module: module.into(),
                name: import.into(),
            },
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if is_clean_capability_name(&self.name) {
            Ok(())
        } else {
            Err(format!(
                "capability '{}' is not a clean versionless snake-case name",
                self.name
            ))
        }
    }
}

/// A capability binding accepted by the active world profile. `shimmed` is
/// true when the engine supports a newer `contract_rev` than the mod's floor
/// and therefore runs the older contract through the compatibility shim.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapabilityUse {
    pub name: String,
    pub min_rev: Option<u32>,
    pub supported_rev: u32,
    pub shimmed: bool,
    pub optional: bool,
    pub sources: Vec<RequirementSource>,
}

/// Runtime target and registry facts a world has pinned. The constructors use
/// one engine contract table for both targets — target version changes game
/// facts, not the ABI revision — while callers can edit the public maps to
/// describe future/custom profiles.
#[derive(Clone, Debug, PartialEq)]
pub struct WorldProfile {
    pub target_version: String,
    pub worldgen_epoch: String,
    /// Clean capability name -> supported internal `contract_rev`.
    pub capabilities: BTreeMap<String, u32>,
    /// Extensible vanilla-id baseline. An empty set disables the baseline
    /// check for profiles that carry a complete registry elsewhere.
    pub known_vanilla_ids: BTreeSet<String>,
}

impl WorldProfile {
    pub fn vanilla_1_21_1() -> Self {
        Self::vanilla("1.21.1", "mc-1.21.1-v1", false)
    }

    pub fn vanilla_26_3() -> Self {
        Self::vanilla("26.3", "mc-26.3-v1", true)
    }

    fn vanilla(target: &str, epoch: &str, future: bool) -> Self {
        let capabilities = [
            "block_registry",
            "recipe",
            "fluid_bus",
            "item_bus",
            "energy_bus",
            "mana_bus",
            "worldgen",
            "loot_modifiers",
            "client_hud",
            "entity_registry",
            "datapack",
            "save_format",
            "chat",
        ]
        .into_iter()
        .map(|name| (name.to_string(), 1))
        .collect();
        let mut known_vanilla_ids = [
            "minecraft:air",
            "minecraft:stone",
            "minecraft:dirt",
            "minecraft:water",
            "minecraft:lava",
            "minecraft:oak_log",
            "minecraft:oak_planks",
            "minecraft:stick",
            "minecraft:coal",
            "minecraft:iron_ore",
            "minecraft:iron_ingot",
            "minecraft:copper_ore",
            "minecraft:copper_ingot",
            "minecraft:gold_ingot",
            "minecraft:diamond",
            "minecraft:crafting_table",
            "minecraft:furnace",
            "minecraft:chest",
            "minecraft:apple",
            "minecraft:bread",
            "minecraft:zombie",
            "minecraft:skeleton",
            "minecraft:cow",
            "minecraft:pig",
            "minecraft:sheep",
            "minecraft:chicken",
            "minecraft:wolf",
            "minecraft:creeper",
            "minecraft:spider",
            "minecraft:enderman",
            "minecraft:player",
            "minecraft:overworld",
            "minecraft:the_nether",
            "minecraft:the_end",
            "minecraft:plains",
            "minecraft:forest",
            "minecraft:desert",
            "minecraft:river",
            "minecraft:ocean",
            "minecraft:mountains",
            "minecraft:windswept_hills",
            "minecraft:cobblestone",
            "minecraft:gravel",
            "minecraft:sand",
            "minecraft:sandstone",
            "minecraft:grass_block",
            "minecraft:oak_leaves",
            "minecraft:oak_sapling",
            "minecraft:spruce_log",
            "minecraft:birch_log",
            "minecraft:glass",
            "minecraft:iron_block",
            "minecraft:gold_block",
            "minecraft:diamond_block",
            "minecraft:redstone",
            "minecraft:redstone_block",
            "minecraft:torch",
            "minecraft:bucket",
            "minecraft:water_bucket",
            "minecraft:lava_bucket",
            "minecraft:wheat",
            "minecraft:wheat_seeds",
            "minecraft:carrot",
            "minecraft:potato",
            "minecraft:leather",
            "minecraft:paper",
            "minecraft:book",
            "minecraft:emerald",
            "minecraft:flint",
            "minecraft:gunpowder",
            "minecraft:string",
            "minecraft:bone",
            "minecraft:egg",
            "minecraft:sugar",
            "minecraft:cocoa_beans",
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<BTreeSet<_>>();
        if future {
            for id in [
                "minecraft:short_grass",
                "minecraft:oak_boat",
                "minecraft:oak_chest_boat",
                "minecraft:mushroom_cow",
                "minecraft:snow_layer",
                // Curated in state-store's 1.21.1 -> 26.3 DataFixer as
                // entries with no 1.21.1 equivalent.
                "minecraft:dried_ghast",
                "minecraft:ghastling",
                "minecraft:happy_ghast",
                "minecraft:music_disc_tears",
            ] {
                known_vanilla_ids.insert(id.to_string());
            }
        } else {
            for id in [
                "minecraft:grass",
                "minecraft:boat",
                "minecraft:chest_boat",
                "minecraft:mooshroom",
                "minecraft:snow",
            ] {
                known_vanilla_ids.insert(id.to_string());
            }
        }
        Self {
            target_version: target.to_string(),
            worldgen_epoch: epoch.to_string(),
            capabilities,
            known_vanilla_ids,
        }
    }
}

/// Machine-readable family of one independently loadable feature unit.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum FeatureKind {
    Recipe,
    CrossModIntegration,
    ClientCosmetic,
    Worldgen,
    LootModifier,
    CoreGameplay,
    Data,
    Custom(String),
}

impl FeatureKind {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Recipe => "recipe",
            Self::CrossModIntegration => "cross_mod_integration",
            Self::ClientCosmetic => "client_cosmetic",
            Self::Worldgen => "worldgen",
            Self::LootModifier => "loot_modifier",
            Self::CoreGameplay => "core_gameplay",
            Self::Data => "data",
            Self::Custom(kind) => kind.as_str(),
        }
    }

    fn from_declared(kind: &str) -> Self {
        match kind {
            "recipe" => Self::Recipe,
            "cross_mod_integration" => Self::CrossModIntegration,
            "client_cosmetic" => Self::ClientCosmetic,
            "worldgen" => Self::Worldgen,
            "loot_modifier" => Self::LootModifier,
            "core_gameplay" => Self::CoreGameplay,
            "data" => Self::Data,
            other => Self::Custom(other.to_string()),
        }
    }
}

/// One candidate feature boundary. Only a feature that is optional, isolated,
/// non-worldgen, non-core-state, and in a proven feature-scoped family may be
/// skipped for a `Partial` verdict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeatureUnit {
    pub id: String,
    pub kind: FeatureKind,
    pub optional: bool,
    pub isolated: bool,
    pub affects_worldgen: bool,
    pub affects_core_state: bool,
    pub cross_mod_integration: bool,
    pub cosmetic_only: bool,
    pub required_capabilities: Vec<CapabilityRequirement>,
    pub referenced_ids: BTreeSet<String>,
    /// Exact path/symbol evidence for the feature boundary.
    pub evidence: String,
}

impl FeatureUnit {
    pub fn new(id: impl Into<String>, kind: FeatureKind, evidence: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            kind,
            optional: false,
            isolated: false,
            affects_worldgen: false,
            affects_core_state: true,
            cross_mod_integration: false,
            cosmetic_only: false,
            required_capabilities: Vec::new(),
            referenced_ids: BTreeSet::new(),
            evidence: evidence.into(),
        }
    }

    pub fn isolated_recipe(id: impl Into<String>, evidence: impl Into<String>) -> Self {
        let mut feature = Self::new(id, FeatureKind::Recipe, evidence);
        feature.optional = true;
        feature.isolated = true;
        feature.affects_core_state = false;
        feature
    }

    pub fn strictly_isolated_optional(&self) -> bool {
        self.optional
            && self.isolated
            && !self.affects_worldgen
            && !self.affects_core_state
            && (matches!(
                self.kind,
                FeatureKind::Recipe
                    | FeatureKind::CrossModIntegration
                    | FeatureKind::ClientCosmetic
            ) || self.cross_mod_integration
                || self.cosmetic_only)
    }
}

/// One live-core Mixin transformation that cannot be lifted onto a standard
/// event hook.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct CoreMixinIssue {
    pub mixin_class: String,
    pub target_class: String,
    pub target_method: String,
    pub annotation: String,
    pub reason: String,
    pub evidence: String,
}

/// One non-Mixin core schema/execution surface that structural lifting cannot
/// represent safely (for example a custom Java chunk generator or a mutating
/// access transformer).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct CoreSchemaIssue {
    pub surface: String,
    pub evidence: String,
    pub message: String,
}

/// Complete structural facts extracted from one declarative bundle or raw
/// archive. All fields are public so embedders can inspect/extend the facts
/// before evaluation; evaluation itself remains deterministic and fail-closed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InspectableModUnit {
    pub mod_id: String,
    pub declared_requirements: Vec<CapabilityRequirement>,
    pub inferred_requirements: Vec<CapabilityRequirement>,
    pub features: Vec<FeatureUnit>,
    pub worldgen_epoch: Option<String>,
    pub modifies_worldgen: bool,
    pub core_mixin_issues: Vec<CoreMixinIssue>,
    pub core_schema_issues: Vec<CoreSchemaIssue>,
    /// Namespaces whose content is supplied inside this unit. Other
    /// non-`minecraft` namespaces are unresolved cross-mod references unless
    /// the world profile is extended by the caller.
    pub own_namespaces: BTreeSet<String>,
}

impl InspectableModUnit {
    pub fn empty(mod_id: impl Into<String>) -> Self {
        let mod_id = mod_id.into();
        let mut own_namespaces = BTreeSet::new();
        if !mod_id.is_empty() {
            own_namespaces.insert(mod_id.clone());
        }
        Self {
            mod_id,
            declared_requirements: Vec::new(),
            inferred_requirements: Vec::new(),
            features: Vec::new(),
            worldgen_epoch: None,
            modifies_worldgen: false,
            core_mixin_issues: Vec::new(),
            core_schema_issues: Vec::new(),
            own_namespaces,
        }
    }

    /// Inspect an already parsed declarative bundle. New optional v1 metadata
    /// (`requires`, `optional_features`, `worldgen_epoch`) is consumed when
    /// present; bundle contents are still structurally inferred either way.
    pub fn from_bundle(bundle: &DeclarativeModBundle) -> Self {
        let mut unit = Self::empty(&bundle.mod_id);
        for req in &bundle.requires {
            unit.declared_requirements
                .push(map_bundle_requirement(req, "requires"));
        }
        unit.worldgen_epoch = bundle.worldgen_epoch.clone();

        for feature in &bundle.optional_features {
            let mut out = FeatureUnit::new(
                &feature.id,
                FeatureKind::from_declared(&feature.kind),
                format!("optional_features:{}", feature.id),
            );
            out.optional = true;
            out.isolated = feature.isolated;
            out.affects_worldgen = feature.affects_worldgen;
            out.affects_core_state = feature.affects_core_state;
            out.cross_mod_integration = feature.cross_mod;
            out.cosmetic_only = feature.cosmetic;
            out.required_capabilities = feature
                .requires
                .iter()
                .map(|req| map_bundle_requirement(req, "optional_features[].requires"))
                .collect();
            out.referenced_ids = feature
                .references
                .iter()
                .filter_map(|id| normalize_resource_reference(id))
                .collect();
            unit.features.push(out);
        }

        for rule in &bundle.portals {
            let mut feature = FeatureUnit::new(
                format!("portal:{}", rule.name),
                FeatureKind::CoreGameplay,
                format!("portals:{}", rule.name),
            );
            if let Some(id) = normalize_resource_reference(&rule.target_dim) {
                feature.referenced_ids.insert(id);
            }
            match &rule.trigger {
                PortalTrigger::VerticalFrame {
                    frame_block,
                    igniter_item,
                    portal_block,
                    ..
                } => {
                    for id in [frame_block, igniter_item, portal_block] {
                        if let Some(id) = normalize_resource_reference(id) {
                            feature.referenced_ids.insert(id);
                        }
                    }
                }
                PortalTrigger::HorizontalPool {
                    border_tags_or_blocks,
                    fluid_block,
                    drop_item,
                    portal_block,
                    ..
                } => {
                    for id in
                        border_tags_or_blocks
                            .iter()
                            .chain([fluid_block, drop_item, portal_block])
                    {
                        if let Some(id) = normalize_resource_reference(id) {
                            feature.referenced_ids.insert(id);
                        }
                    }
                }
                PortalTrigger::AltitudeThreshold {
                    required_vehicle_or_item,
                    ..
                } => {
                    if let Some(id) = required_vehicle_or_item
                        .as_deref()
                        .and_then(normalize_resource_reference)
                    {
                        feature.referenced_ids.insert(id);
                    }
                }
                PortalTrigger::DirectBlockInteract {
                    block_id,
                    required_item,
                } => {
                    if let Some(id) = normalize_resource_reference(block_id) {
                        feature.referenced_ids.insert(id);
                    }
                    if let Some(id) = required_item
                        .as_deref()
                        .and_then(normalize_resource_reference)
                    {
                        feature.referenced_ids.insert(id);
                    }
                }
            }
            feature
                .required_capabilities
                .push(CapabilityRequirement::inferred_bundle(
                    "block_registry",
                    None,
                    false,
                    format!("portals:{}", rule.name),
                ));
            unit.features.push(feature);
        }
        for creature in &bundle.creatures {
            let mut feature = FeatureUnit::new(
                format!("creature:{}", creature.entity_type),
                FeatureKind::CoreGameplay,
                format!("creatures:{}", creature.entity_type),
            );
            for id in [&creature.entity_type]
                .into_iter()
                .chain(creature.tame_items.iter())
                .chain(creature.breed_items.iter())
                .chain(creature.heal_items.iter().map(|(id, _)| id))
            {
                if let Some(id) = normalize_resource_reference(id) {
                    feature.referenced_ids.insert(id);
                }
            }
            feature
                .required_capabilities
                .push(CapabilityRequirement::inferred_bundle(
                    "entity_registry",
                    None,
                    false,
                    format!("creatures:{}", creature.entity_type),
                ));
            unit.features.push(feature);
        }
        for region in &bundle.biome_regions {
            let mut feature = FeatureUnit::new(
                format!("biome_region:{}", region.dimension),
                FeatureKind::Worldgen,
                format!("biome_regions:{}", region.dimension),
            );
            feature.affects_worldgen = true;
            if let Some(id) = normalize_resource_reference(&region.dimension) {
                feature.referenced_ids.insert(id);
            }
            for (from, to) in &region.biome_overrides {
                for id in [from, to] {
                    if let Some(id) = normalize_resource_reference(id) {
                        feature.referenced_ids.insert(id);
                    }
                }
            }
            feature
                .required_capabilities
                .push(CapabilityRequirement::inferred_bundle(
                    "worldgen",
                    None,
                    false,
                    format!("biome_regions:{}", region.dimension),
                ));
            unit.features.push(feature);
        }

        if !bundle.biome_regions.is_empty() {
            unit.modifies_worldgen = true;
            unit.add_inferred(CapabilityRequirement::inferred_bundle(
                "worldgen",
                None,
                false,
                "biome_regions",
            ));
        }
        if !bundle.creatures.is_empty() {
            unit.add_inferred(CapabilityRequirement::inferred_bundle(
                "entity_registry",
                None,
                false,
                "creatures",
            ));
        }
        if !bundle.spells.is_empty()
            || !bundle.custom_statuses.is_empty()
            || !bundle.rituals.is_empty()
        {
            unit.add_inferred(CapabilityRequirement::inferred_bundle(
                "mana_bus",
                None,
                false,
                "spells/custom_statuses/rituals",
            ));
        }
        if !bundle.portals.is_empty() {
            unit.add_inferred(CapabilityRequirement::inferred_bundle(
                "block_registry",
                None,
                false,
                "portals",
            ));
        }
        unit.sort_deterministic();
        unit
    }

    /// Parse and inspect a `megu-mod-bundle/1` JSON document.
    pub fn from_bundle_json(text: &str) -> Result<Self, StructuralCompatError> {
        let bundle = DeclarativeModBundle::from_json(text)
            .map_err(|e| StructuralCompatError::Manifest(e.to_string()))?;
        Ok(Self::from_bundle(&bundle))
    }

    /// Inspect a raw JAR/ZIP archive in memory. This reuses the crate's
    /// fail-closed ZIP reader, Mixin bytecode scanner, loader-extension
    /// scanner, and generic datapack extractor; no JVM or external unzipper is
    /// launched.
    pub fn from_jar_bytes(jar_bytes: &[u8], display: &str) -> Result<Self, StructuralCompatError> {
        let entries = zip_member_list(jar_bytes, display)
            .map_err(|e| StructuralCompatError::Archive(e.reason().to_string()))?;
        if entries.len() > MAX_BYTECODE_MEMBERS {
            return Err(StructuralCompatError::Validation(format!(
                "{display}: member count exceeds {MAX_BYTECODE_MEMBERS}"
            )));
        }

        let extensions = inspect_jar_extensions(jar_bytes, display)
            .map_err(|e| StructuralCompatError::Manifest(e.reason().to_string()))?;

        let has_loader_manifest = entries.iter().any(|entry| {
            matches!(
                entry.name.as_str(),
                "fabric.mod.json"
                    | "quilt.mod.json"
                    | "META-INF/neoforge.mods.toml"
                    | "META-INF/mods.toml"
            )
        });
        let descriptor = if has_loader_manifest {
            Some(
                scan_mod_jar_named(jar_bytes, display)
                    .map_err(|e| StructuralCompatError::Manifest(e.reason().to_string()))?,
            )
        } else {
            None
        };

        let mut embedded_unit = None;
        if let Some(entry) = entries
            .iter()
            .find(|entry| entry.name == EMBEDDED_BUNDLE_MEMBER)
        {
            let bytes = read_zip_member(jar_bytes, entry, display)
                .map_err(|e| StructuralCompatError::Archive(e.reason().to_string()))?;
            let text = String::from_utf8(bytes).map_err(|_| {
                StructuralCompatError::Manifest(format!(
                    "{display}: {EMBEDDED_BUNDLE_MEMBER} is not UTF-8"
                ))
            })?;
            embedded_unit = Some(Self::from_bundle_json(&text)?);
        }

        let fallback_mod_id = descriptor
            .as_ref()
            .map(|d| d.mod_id.clone())
            .filter(|id| !id.is_empty())
            .or_else(|| embedded_unit.as_ref().map(|u| u.mod_id.clone()))
            .unwrap_or_else(|| fallback_mod_id(display));
        let mut unit = Self::empty(fallback_mod_id);
        if let Some(embedded) = embedded_unit {
            unit.merge(embedded);
        }
        if let Some(descriptor) = &descriptor {
            for id in &descriptor.declared_mod_ids {
                unit.own_namespaces.insert(id.clone());
            }
        }

        let data = extract_mod_data(jar_bytes, display).map_err(StructuralCompatError::Archive)?;
        for namespace in &data.namespaces {
            unit.own_namespaces.insert(namespace.clone());
        }

        let wg = &data.dimensions.worldgen;
        let has_worldgen_data = !wg.noise_settings.is_empty()
            || !wg.biomes.is_empty()
            || !wg.configured_features.is_empty()
            || !wg.placed_features.is_empty()
            || !wg.structures.is_empty()
            || !data.dimensions.dimensions.is_empty()
            || !data.dimensions.dimension_types.is_empty()
            || !data.biome_modifiers.is_empty();
        if has_worldgen_data {
            unit.modifies_worldgen = true;
            unit.add_inferred(CapabilityRequirement::inferred_datapack(
                "worldgen",
                None,
                false,
                "data/*/worldgen|dimension|neoforge/biome_modifier",
            ));
        }
        for modifier in &data.biome_modifiers {
            if matches!(
                modifier.kind,
                BiomeModifierKind::AddFeatures
                    | BiomeModifierKind::RemoveFeatures
                    | BiomeModifierKind::AddSpawns
                    | BiomeModifierKind::RemoveSpawns
            ) {
                unit.modifies_worldgen = true;
            }
        }
        for custom in data.dimensions.dimensions.iter().filter_map(|dimension| {
            match &dimension.generator_kind {
                crate::dimension_mod::GeneratorKind::CustomChunkGenerator(kind) => {
                    Some((dimension.id.clone(), kind.clone()))
                }
                _ => None,
            }
        }) {
            unit.core_schema_issues.push(CoreSchemaIssue {
                surface: "custom_chunk_generator".to_string(),
                evidence: custom.0,
                message: format!(
                    "custom Java chunk generator '{}' cannot run structurally",
                    custom.1
                ),
            });
        }
        if !data.loot_tables.is_empty() {
            unit.add_inferred(CapabilityRequirement::inferred_datapack(
                "datapack",
                None,
                false,
                "data/*/loot_tables",
            ));
        }

        // Mixin bytecode scan: live un-liftable transformations are core
        // blockers; legacy-save transformations are recorded as schema
        // issues; cosmetic client sites become isolated optional features.
        let mixin_report = scan_jar_mixins_with_options(jar_bytes, JarScanOptions::default())
            .map_err(|e| StructuralCompatError::Validation(e.to_string()))?;
        collect_mixin_issues(&mixin_report.verdict, &mut unit);
        for site in &mixin_report.legacy_datafix_sites {
            unit.core_schema_issues.push(CoreSchemaIssue {
                surface: "legacy_save_mixin".to_string(),
                evidence: mixin_evidence(site),
                message: format!(
                    "{} transformation on a legacy save target cannot be schema-proven",
                    site.annotation.as_str()
                ),
            });
        }
        for (index, site) in mixin_report.client_cosmetic_sites.iter().enumerate() {
            let mut feature = FeatureUnit::new(
                format!("client_cosmetic:{}:{index}", site.mixin_class),
                FeatureKind::ClientCosmetic,
                mixin_evidence(site),
            );
            feature.optional = true;
            feature.isolated = true;
            feature.affects_core_state = false;
            feature.cosmetic_only = true;
            feature
                .required_capabilities
                .push(CapabilityRequirement::inferred_bytecode(
                    "client_hud",
                    None,
                    true,
                    format!("{}#{}", site.mixin_class, site.target_method),
                ));
            unit.features.push(feature);
        }

        // Access transformers that need JVM-wide semantics are not a core
        // schema proof. Embedded Jar-in-Jar content itself is recursively
        // inspected by `extract_mod_data` and is not treated as a blocker.
        for at in &extensions.access_transformers {
            if at.requires_jvm_bridge() {
                unit.core_schema_issues.push(CoreSchemaIssue {
                    surface: "access_transformer".to_string(),
                    evidence: format!(
                        "accesstransformer.cfg:{}:{}",
                        at.owner_class,
                        at.member_name.as_deref().unwrap_or("<class>")
                    ),
                    message: "access transformer requires JVM bridge semantics".to_string(),
                });
            }
        }

        scan_data_json_features(jar_bytes, display, &entries, &mut unit)?;
        scan_bytecode_capabilities(jar_bytes, display, &entries, &mut unit)?;
        scan_wasm_imports(jar_bytes, display, &entries, &mut unit)?;
        scan_native_manifest(jar_bytes, display, &entries, &mut unit)?;

        unit.sort_deterministic();
        Ok(unit)
    }

    /// Evaluate this unit against the active world profile. The verdict is
    /// deterministic and total: every destructive ambiguity is fail-closed.
    pub fn evaluate(&self, profile: &WorldProfile) -> CompatLoadVerdict {
        evaluate_unit(self, profile)
    }

    fn add_inferred(&mut self, requirement: CapabilityRequirement) {
        if !self.inferred_requirements.iter().any(|existing| {
            existing.name == requirement.name && existing.source == requirement.source
        }) {
            self.inferred_requirements.push(requirement);
        }
    }

    fn merge(&mut self, other: Self) {
        if self.mod_id.starts_with("unknown") && !other.mod_id.is_empty() {
            self.mod_id = other.mod_id.clone();
        }
        self.declared_requirements
            .extend(other.declared_requirements);
        self.inferred_requirements
            .extend(other.inferred_requirements);
        self.features.extend(other.features);
        if self.worldgen_epoch.is_none() {
            self.worldgen_epoch = other.worldgen_epoch;
        }
        self.modifies_worldgen |= other.modifies_worldgen;
        self.core_mixin_issues.extend(other.core_mixin_issues);
        self.core_schema_issues.extend(other.core_schema_issues);
        self.own_namespaces.extend(other.own_namespaces);
    }

    fn sort_deterministic(&mut self) {
        let source_key = |req: &CapabilityRequirement| {
            (
                req.name.clone(),
                req.source.clone(),
                req.min_rev,
                req.optional,
            )
        };
        self.declared_requirements.sort_by_key(source_key);
        self.inferred_requirements.sort_by_key(source_key);
        self.features
            .sort_by(|a, b| a.id.cmp(&b.id).then(a.evidence.cmp(&b.evidence)));
        self.core_mixin_issues.sort();
        self.core_schema_issues.sort();
    }
}

/// Convenience free function mirroring [`InspectableModUnit::from_jar_bytes`].
pub fn inspect_jar_bytes(
    jar_bytes: &[u8],
    display: &str,
) -> Result<InspectableModUnit, StructuralCompatError> {
    InspectableModUnit::from_jar_bytes(jar_bytes, display)
}

/// Why loading must stop before world mutation.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CompatBlockerKind {
    WorldgenEpochMismatch,
    MissingWorldgenEpoch,
    MissingCapability,
    UnsupportedContractRev,
    InvalidCapabilityName,
    UnliftableCoreMixin,
    UnsupportedCoreSchema,
    UnsupportedReference,
}

impl CompatBlockerKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::WorldgenEpochMismatch => "worldgen_epoch_mismatch",
            Self::MissingWorldgenEpoch => "missing_worldgen_epoch",
            Self::MissingCapability => "missing_capability",
            Self::UnsupportedContractRev => "unsupported_contract_rev",
            Self::InvalidCapabilityName => "invalid_capability_name",
            Self::UnliftableCoreMixin => "unliftable_core_mixin",
            Self::UnsupportedCoreSchema => "unsupported_core_schema",
            Self::UnsupportedReference => "unsupported_reference",
        }
    }
}

/// Exact machine- and operator-actionable blocker evidence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompatBlocker {
    pub kind: CompatBlockerKind,
    pub feature_id: Option<String>,
    pub capability: Option<String>,
    pub required_rev: Option<u32>,
    pub supported_rev: Option<u32>,
    pub evidence: String,
    pub message: String,
}

impl CompatBlocker {
    fn new(
        kind: CompatBlockerKind,
        evidence: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            feature_id: None,
            capability: None,
            required_rev: None,
            supported_rev: None,
            evidence: evidence.into(),
            message: message.into(),
        }
    }
}

/// One optional feature omitted by a `Partial` load.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkippedFeature {
    pub feature_id: String,
    pub kind: String,
    pub reason: String,
    pub evidence: String,
    pub unsupported_capabilities: Vec<String>,
    pub unsupported_references: Vec<String>,
}

/// Deterministic three-tier safe-load verdict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompatLoadVerdict {
    Full {
        capabilities_used: Vec<CapabilityUse>,
        inferred_count: usize,
        declared_count: usize,
    },
    Partial {
        skipped_features: Vec<SkippedFeature>,
        active_features: Vec<String>,
        reason_summary: String,
    },
    Blocked {
        blockers: Vec<CompatBlocker>,
    },
}

impl CompatLoadVerdict {
    pub fn tier(&self) -> &'static str {
        match self {
            Self::Full { .. } => "full",
            Self::Partial { .. } => "partial",
            Self::Blocked { .. } => "blocked",
        }
    }

    /// Deterministic compact JSON with explicit `schema_version: 1`.
    pub fn to_json(&self) -> String {
        let mut fields = vec![
            (
                "schema_version".to_string(),
                WireJson::Number(f64::from(STRUCTURAL_COMPAT_SCHEMA_VERSION)),
            ),
            (
                "verdict".to_string(),
                WireJson::String(self.tier().to_string()),
            ),
        ];
        match self {
            Self::Full {
                capabilities_used,
                inferred_count,
                declared_count,
            } => {
                fields.push((
                    "capabilities_used".to_string(),
                    WireJson::Array(capabilities_used.iter().map(capability_use_json).collect()),
                ));
                fields.push((
                    "inferred_count".to_string(),
                    WireJson::Number(*inferred_count as f64),
                ));
                fields.push((
                    "declared_count".to_string(),
                    WireJson::Number(*declared_count as f64),
                ));
            }
            Self::Partial {
                skipped_features,
                active_features,
                reason_summary,
            } => {
                fields.push((
                    "active_features".to_string(),
                    WireJson::Array(
                        active_features
                            .iter()
                            .map(|s| WireJson::String(s.clone()))
                            .collect(),
                    ),
                ));
                fields.push((
                    "skipped_features".to_string(),
                    WireJson::Array(skipped_features.iter().map(skipped_feature_json).collect()),
                ));
                fields.push((
                    "reason_summary".to_string(),
                    WireJson::String(reason_summary.clone()),
                ));
            }
            Self::Blocked { blockers } => {
                fields.push((
                    "blockers".to_string(),
                    WireJson::Array(blockers.iter().map(blocker_json).collect()),
                ));
            }
        }
        WireJson::Object(fields).to_json_string()
    }
}

fn capability_use_json(usage: &CapabilityUse) -> WireJson {
    WireJson::Object(vec![
        ("name".to_string(), WireJson::String(usage.name.clone())),
        (
            "min_rev".to_string(),
            usage
                .min_rev
                .map_or(WireJson::Null, |v| WireJson::Number(f64::from(v))),
        ),
        (
            "supported_rev".to_string(),
            WireJson::Number(f64::from(usage.supported_rev)),
        ),
        ("shimmed".to_string(), WireJson::Bool(usage.shimmed)),
        ("optional".to_string(), WireJson::Bool(usage.optional)),
        (
            "sources".to_string(),
            WireJson::Array(usage.sources.iter().map(requirement_source_json).collect()),
        ),
    ])
}

fn requirement_source_json(source: &RequirementSource) -> WireJson {
    let mut fields = vec![
        (
            "kind".to_string(),
            WireJson::String(source.kind().to_string()),
        ),
        ("evidence".to_string(), WireJson::String(source.evidence())),
    ];
    match source {
        RequirementSource::DeclaredManifest { field } => {
            fields.push(("field".to_string(), WireJson::String(field.clone())));
        }
        RequirementSource::InferredBytecode { symbol } => {
            fields.push(("symbol".to_string(), WireJson::String(symbol.clone())));
        }
        RequirementSource::InferredDatapack { path }
        | RequirementSource::InferredBundle { path } => {
            fields.push(("path".to_string(), WireJson::String(path.clone())));
        }
        RequirementSource::InferredWasmImport { module, name } => {
            fields.push(("module".to_string(), WireJson::String(module.clone())));
            fields.push(("name".to_string(), WireJson::String(name.clone())));
        }
    }
    WireJson::Object(fields)
}

fn skipped_feature_json(feature: &SkippedFeature) -> WireJson {
    WireJson::Object(vec![
        (
            "feature_id".to_string(),
            WireJson::String(feature.feature_id.clone()),
        ),
        ("kind".to_string(), WireJson::String(feature.kind.clone())),
        (
            "reason".to_string(),
            WireJson::String(feature.reason.clone()),
        ),
        (
            "evidence".to_string(),
            WireJson::String(feature.evidence.clone()),
        ),
        (
            "unsupported_capabilities".to_string(),
            WireJson::Array(
                feature
                    .unsupported_capabilities
                    .iter()
                    .map(|s| WireJson::String(s.clone()))
                    .collect(),
            ),
        ),
        (
            "unsupported_references".to_string(),
            WireJson::Array(
                feature
                    .unsupported_references
                    .iter()
                    .map(|s| WireJson::String(s.clone()))
                    .collect(),
            ),
        ),
    ])
}

fn blocker_json(blocker: &CompatBlocker) -> WireJson {
    WireJson::Object(vec![
        (
            "kind".to_string(),
            WireJson::String(blocker.kind.as_str().to_string()),
        ),
        (
            "feature_id".to_string(),
            blocker
                .feature_id
                .clone()
                .map_or(WireJson::Null, WireJson::String),
        ),
        (
            "capability".to_string(),
            blocker
                .capability
                .clone()
                .map_or(WireJson::Null, WireJson::String),
        ),
        (
            "required_rev".to_string(),
            blocker
                .required_rev
                .map_or(WireJson::Null, |v| WireJson::Number(f64::from(v))),
        ),
        (
            "supported_rev".to_string(),
            blocker
                .supported_rev
                .map_or(WireJson::Null, |v| WireJson::Number(f64::from(v))),
        ),
        (
            "evidence".to_string(),
            WireJson::String(blocker.evidence.clone()),
        ),
        (
            "message".to_string(),
            WireJson::String(blocker.message.clone()),
        ),
    ])
}

// ---------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct ConsolidatedRequirement {
    name: String,
    min_rev: Option<u32>,
    optional: bool,
    sources: BTreeSet<RequirementSource>,
}

fn consolidate<'a>(
    requirements: impl IntoIterator<Item = &'a CapabilityRequirement>,
) -> Vec<ConsolidatedRequirement> {
    let mut by_name: BTreeMap<String, ConsolidatedRequirement> = BTreeMap::new();
    for req in requirements {
        let entry = by_name
            .entry(req.name.clone())
            .or_insert_with(|| ConsolidatedRequirement {
                name: req.name.clone(),
                min_rev: req.min_rev,
                optional: true,
                sources: BTreeSet::new(),
            });
        entry.min_rev = match (entry.min_rev, req.min_rev) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        };
        entry.optional &= req.optional;
        entry.sources.insert(req.source.clone());
    }
    by_name.into_values().collect()
}

fn merge_usage(
    uses: &mut BTreeMap<String, CapabilityUse>,
    req: &ConsolidatedRequirement,
    supported_rev: u32,
) {
    let mut sources: Vec<_> = req.sources.iter().cloned().collect();
    sources.sort();
    uses.entry(req.name.clone())
        .and_modify(|existing| {
            existing.min_rev = match (existing.min_rev, req.min_rev) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            };
            existing.optional &= req.optional;
            existing.sources.extend(sources.clone());
            existing.sources.sort();
            existing.sources.dedup();
            existing.shimmed = existing
                .min_rev
                .is_some_and(|min| existing.supported_rev > min);
        })
        .or_insert_with(|| CapabilityUse {
            name: req.name.clone(),
            min_rev: req.min_rev,
            supported_rev,
            shimmed: req.min_rev.is_some_and(|min| supported_rev > min),
            optional: req.optional,
            sources,
        });
}

fn capability_gap(
    req: &ConsolidatedRequirement,
    profile: &WorldProfile,
) -> Option<(CompatBlockerKind, Option<u32>)> {
    if !is_clean_capability_name(&req.name) {
        return Some((CompatBlockerKind::InvalidCapabilityName, None));
    }
    match profile.capabilities.get(&req.name) {
        None => Some((CompatBlockerKind::MissingCapability, None)),
        Some(&supported) if req.min_rev.is_some_and(|min| min > supported) => {
            Some((CompatBlockerKind::UnsupportedContractRev, Some(supported)))
        }
        Some(_) => None,
    }
}

fn capability_blocker(
    kind: CompatBlockerKind,
    req: &ConsolidatedRequirement,
    supported_rev: Option<u32>,
    feature_id: Option<String>,
) -> CompatBlocker {
    let evidence = req
        .sources
        .iter()
        .next()
        .map(RequirementSource::evidence)
        .unwrap_or_else(|| "<unknown>".to_string());
    let message = match kind {
        CompatBlockerKind::InvalidCapabilityName => format!(
            "capability '{}' embeds a version suffix or is not clean snake-case",
            req.name
        ),
        CompatBlockerKind::MissingCapability => {
            format!(
                "required capability '{}' is not supported by this world",
                req.name
            )
        }
        CompatBlockerKind::UnsupportedContractRev => format!(
            "capability '{}' requires contract_rev {} but this world supports {}",
            req.name,
            req.min_rev.unwrap_or(0),
            supported_rev.unwrap_or(0)
        ),
        _ => unreachable!("not a capability blocker kind"),
    };
    let mut blocker = CompatBlocker::new(kind, evidence, message);
    blocker.feature_id = feature_id;
    blocker.capability = Some(req.name.clone());
    blocker.required_rev = req.min_rev;
    blocker.supported_rev = supported_rev;
    blocker
}

fn unsupported_references(
    unit: &InspectableModUnit,
    feature: &FeatureUnit,
    profile: &WorldProfile,
) -> Vec<String> {
    let mut out = BTreeSet::new();
    for id in &feature.referenced_ids {
        if id.starts_with('#') {
            continue;
        }
        let (namespace, _) = id.split_once(':').unwrap_or(("minecraft", id.as_str()));
        if namespace == "minecraft" {
            if !profile.known_vanilla_ids.is_empty() && !profile.known_vanilla_ids.contains(id) {
                out.insert(id.clone());
            }
        } else if !unit.own_namespaces.contains(namespace) {
            out.insert(id.clone());
        }
    }
    out.into_iter().collect()
}

fn evaluate_unit(unit: &InspectableModUnit, profile: &WorldProfile) -> CompatLoadVerdict {
    let mut blockers = Vec::new();
    let mut skipped = Vec::new();
    let mut active_features = BTreeSet::new();
    let mut uses: BTreeMap<String, CapabilityUse> = BTreeMap::new();

    if unit.modifies_worldgen {
        match &unit.worldgen_epoch {
            Some(epoch) if epoch != &profile.worldgen_epoch => {
                blockers.push(CompatBlocker::new(
                    CompatBlockerKind::WorldgenEpochMismatch,
                    format!("worldgen_epoch:{epoch}"),
                    format!(
                        "mod modifies world generation but requires epoch '{epoch}' while the world pins '{}'",
                        profile.worldgen_epoch
                    ),
                ));
            }
            Some(_) => {}
            None => blockers.push(CompatBlocker::new(
                CompatBlockerKind::MissingWorldgenEpoch,
                "worldgen data without worldgen_epoch",
                "mod modifies world generation but carries no provable worldgen_epoch",
            )),
        }
    }

    for issue in &unit.core_mixin_issues {
        let mut blocker = CompatBlocker::new(
            CompatBlockerKind::UnliftableCoreMixin,
            &issue.evidence,
            format!(
                "un-liftable core mixin {} on {}#{}: {}",
                issue.annotation, issue.target_class, issue.target_method, issue.reason
            ),
        );
        blocker.feature_id = Some(issue.mixin_class.clone());
        blockers.push(blocker);
    }
    for issue in &unit.core_schema_issues {
        blockers.push(CompatBlocker::new(
            CompatBlockerKind::UnsupportedCoreSchema,
            &issue.evidence,
            format!("{}: {}", issue.surface, issue.message),
        ));
    }

    let global_requirements: Vec<_> = unit
        .declared_requirements
        .iter()
        .chain(unit.inferred_requirements.iter())
        .collect();
    for req in consolidate(global_requirements.iter().copied()) {
        match capability_gap(&req, profile) {
            None => merge_usage(
                &mut uses,
                &req,
                *profile.capabilities.get(&req.name).unwrap_or(&0),
            ),
            Some((kind, supported_rev)) => {
                // A bare `optional = true` requirement is useful metadata, but it
                // is not proof of an isolated feature boundary. Without a
                // declared/inferred feature unit there is no bounded thing that
                // can be skipped safely, so even optional global requirements
                // fail closed here.
                blockers.push(capability_blocker(kind, &req, supported_rev, None));
            }
        }
    }

    let mut features = unit.features.clone();
    features.sort_by(|a, b| a.id.cmp(&b.id).then(a.evidence.cmp(&b.evidence)));
    for feature in &features {
        let consolidated = consolidate(&feature.required_capabilities);
        let mut gaps = Vec::new();
        let mut unsupported_caps = BTreeSet::new();
        for req in &consolidated {
            if let Some((kind, supported_rev)) = capability_gap(req, profile) {
                unsupported_caps.insert(req.name.clone());
                gaps.push((kind, req.clone(), supported_rev));
            }
        }
        let unsupported_refs = unsupported_references(unit, feature, profile);
        if gaps.is_empty() && unsupported_refs.is_empty() {
            active_features.insert(feature.id.clone());
            for req in &consolidated {
                merge_usage(
                    &mut uses,
                    req,
                    *profile.capabilities.get(&req.name).unwrap_or(&0),
                );
            }
            continue;
        }

        let mut reasons = Vec::new();
        if !unsupported_caps.is_empty() {
            reasons.push(format!(
                "unsupported capabilities: {}",
                unsupported_caps
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(",")
            ));
        }
        if !unsupported_refs.is_empty() {
            reasons.push(format!(
                "unsupported references: {}",
                unsupported_refs.join(",")
            ));
        }
        let reason = reasons.join("; ");

        if feature.strictly_isolated_optional() {
            skipped.push(SkippedFeature {
                feature_id: feature.id.clone(),
                kind: feature.kind.as_str().to_string(),
                reason,
                evidence: feature.evidence.clone(),
                unsupported_capabilities: unsupported_caps.into_iter().collect(),
                unsupported_references: unsupported_refs,
            });
        } else {
            for (kind, req, supported_rev) in gaps {
                blockers.push(capability_blocker(
                    kind,
                    &req,
                    supported_rev,
                    Some(feature.id.clone()),
                ));
            }
            for reference in unsupported_refs {
                let mut blocker = CompatBlocker::new(
                    CompatBlockerKind::UnsupportedReference,
                    &feature.evidence,
                    format!(
                        "required feature '{}' references unsupported id '{reference}'",
                        feature.id
                    ),
                );
                blocker.feature_id = Some(feature.id.clone());
                blockers.push(blocker);
            }
        }
    }

    blockers.sort_by(|a, b| {
        a.kind
            .cmp(&b.kind)
            .then(a.evidence.cmp(&b.evidence))
            .then(a.message.cmp(&b.message))
    });
    if !blockers.is_empty() {
        return CompatLoadVerdict::Blocked { blockers };
    }

    skipped.sort_by(|a, b| a.feature_id.cmp(&b.feature_id));
    if !skipped.is_empty() {
        let reason_summary = skipped
            .iter()
            .map(|feature| format!("{} ({})", feature.feature_id, feature.reason))
            .collect::<Vec<_>>()
            .join("; ");
        return CompatLoadVerdict::Partial {
            skipped_features: skipped,
            active_features: active_features.into_iter().collect(),
            reason_summary,
        };
    }

    let mut all_requirements = unit
        .declared_requirements
        .iter()
        .chain(unit.inferred_requirements.iter());
    let declared_count = all_requirements
        .by_ref()
        .filter(|req| matches!(req.source, RequirementSource::DeclaredManifest { .. }))
        .count()
        + unit
            .features
            .iter()
            .flat_map(|feature| feature.required_capabilities.iter())
            .filter(|req| matches!(req.source, RequirementSource::DeclaredManifest { .. }))
            .count();
    let inferred_count = unit
        .inferred_requirements
        .iter()
        .filter(|req| !matches!(req.source, RequirementSource::DeclaredManifest { .. }))
        .count()
        + unit
            .features
            .iter()
            .flat_map(|feature| feature.required_capabilities.iter())
            .filter(|req| !matches!(req.source, RequirementSource::DeclaredManifest { .. }))
            .count();
    CompatLoadVerdict::Full {
        capabilities_used: uses.into_values().collect(),
        inferred_count,
        declared_count,
    }
}

// ---------------------------------------------------------------------------
// Bundle / archive fact extraction
// ---------------------------------------------------------------------------

fn map_bundle_requirement(req: &BundleCapabilityRequirement, field: &str) -> CapabilityRequirement {
    CapabilityRequirement::declared_field(&req.name, req.min_rev, req.optional, field)
}

fn fallback_mod_id(display: &str) -> String {
    let stem = display
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(display)
        .trim_end_matches(".jar")
        .trim_end_matches(".zip")
        .trim_end_matches(".mmod");
    let mut out: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-' | '.') {
                c
            } else if c.is_ascii_uppercase() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        out = "unknown_mod".to_string();
    }
    out
}

fn mixin_evidence(site: &MixinSite) -> String {
    format!(
        "{} -> {}#{} {}",
        site.mixin_class, site.target_class, site.target_method, site.annotation
    )
}

fn collect_mixin_issues(verdict: &MixinLiftVerdict, unit: &mut InspectableModUnit) {
    let MixinLiftVerdict::RequiresFallback { unliftable, .. } = verdict else {
        return;
    };
    for (site, reason) in unliftable {
        unit.core_mixin_issues.push(CoreMixinIssue {
            mixin_class: site.mixin_class.clone(),
            target_class: site.target_class.clone(),
            target_method: site.target_method.clone(),
            annotation: site.annotation.as_str().to_string(),
            reason: reason.as_str().to_string(),
            evidence: mixin_evidence(site),
        });
    }
}

fn scan_data_json_features(
    jar_bytes: &[u8],
    display: &str,
    entries: &[crate::modpack::ZipMemberEntry],
    unit: &mut InspectableModUnit,
) -> Result<(), StructuralCompatError> {
    for entry in entries {
        if !entry.name.ends_with(".json") || !entry.name.starts_with("data/") {
            continue;
        }
        let parts: Vec<&str> = entry.name.split('/').collect();
        if parts.len() < 4 {
            continue;
        }
        let namespace = parts[1];
        let category = parts[2];
        let stem = parts[3..].join("/").trim_end_matches(".json").to_string();
        if category == "neoforge" && parts.get(3) == Some(&"biome_modifier") && parts.len() >= 5 {
            let modifier_stem = parts[4..].join("/").trim_end_matches(".json").to_string();
            let bytes = read_zip_member(jar_bytes, entry, display)
                .map_err(|e| StructuralCompatError::Archive(e.reason().to_string()))?;
            let parsed = parse_json(&bytes).map_err(|e| {
                StructuralCompatError::Manifest(format!(
                    "{}: invalid biome modifier JSON: {e:?}",
                    entry.name
                ))
            })?;
            let mut feature = FeatureUnit::new(
                format!("biome_modifier:{namespace}:{modifier_stem}"),
                FeatureKind::Worldgen,
                &entry.name,
            );
            feature.affects_worldgen = true;
            feature.referenced_ids = references_for_keys(
                &parsed,
                &[
                    "biomes",
                    "features",
                    "feature",
                    "entity_types",
                    "entity_type",
                    "block",
                    "blocks",
                ],
            );
            feature
                .required_capabilities
                .push(CapabilityRequirement::inferred_datapack(
                    "worldgen",
                    None,
                    false,
                    &entry.name,
                ));
            unit.features.push(feature);
            continue;
        }
        match category {
            "recipe" | "recipes" => {
                let bytes = read_zip_member(jar_bytes, entry, display)
                    .map_err(|e| StructuralCompatError::Archive(e.reason().to_string()))?;
                let parsed = parse_json(&bytes).map_err(|e| {
                    StructuralCompatError::Manifest(format!(
                        "{}: invalid recipe JSON: {e:?}",
                        entry.name
                    ))
                })?;
                let mut references = BTreeSet::new();
                collect_recipe_references(&parsed, &mut references);
                let optional = recipe_is_optional(&parsed)
                    || entry.name.contains("/compat/")
                    || entry.name.contains("compat_");
                let id = format!("recipe:{namespace}:{stem}");
                let mut feature = FeatureUnit::new(&id, FeatureKind::Recipe, &entry.name);
                feature.optional = optional;
                feature.isolated = true;
                feature.affects_core_state = false;
                feature.referenced_ids = references;
                feature
                    .required_capabilities
                    .push(CapabilityRequirement::inferred_datapack(
                        "recipe",
                        None,
                        optional,
                        &entry.name,
                    ));
                unit.features.push(feature);
            }
            "loot_modifier" | "loot_modifiers" => {
                unit.add_inferred(CapabilityRequirement::inferred_datapack(
                    "loot_modifiers",
                    None,
                    false,
                    &entry.name,
                ));
                let bytes = read_zip_member(jar_bytes, entry, display)
                    .map_err(|e| StructuralCompatError::Archive(e.reason().to_string()))?;
                let parsed = parse_json(&bytes).map_err(|e| {
                    StructuralCompatError::Manifest(format!(
                        "{}: invalid loot modifier JSON: {e:?}",
                        entry.name
                    ))
                })?;
                let mut feature = FeatureUnit::new(
                    format!("loot_modifier:{namespace}:{stem}"),
                    FeatureKind::LootModifier,
                    &entry.name,
                );
                feature.referenced_ids = references_for_keys(
                    &parsed,
                    &[
                        "item",
                        "items",
                        "name",
                        "loot_table",
                        "loot_table_id",
                        "block",
                        "entity",
                    ],
                );
                feature
                    .required_capabilities
                    .push(CapabilityRequirement::inferred_datapack(
                        "loot_modifiers",
                        None,
                        false,
                        &entry.name,
                    ));
                unit.features.push(feature);
            }
            "loot_table" | "loot_tables" => {
                let bytes = read_zip_member(jar_bytes, entry, display)
                    .map_err(|e| StructuralCompatError::Archive(e.reason().to_string()))?;
                let parsed = parse_json(&bytes).map_err(|e| {
                    StructuralCompatError::Manifest(format!(
                        "{}: invalid loot table JSON: {e:?}",
                        entry.name
                    ))
                })?;
                let mut feature = FeatureUnit::new(
                    format!("loot_table:{namespace}:{stem}"),
                    FeatureKind::Data,
                    &entry.name,
                );
                feature.referenced_ids =
                    references_for_keys(&parsed, &["name", "item", "block", "entity"]);
                feature
                    .required_capabilities
                    .push(CapabilityRequirement::inferred_datapack(
                        "datapack",
                        None,
                        false,
                        &entry.name,
                    ));
                unit.features.push(feature);
            }
            "tag" | "tags" => {
                let bytes = read_zip_member(jar_bytes, entry, display)
                    .map_err(|e| StructuralCompatError::Archive(e.reason().to_string()))?;
                let parsed = parse_json(&bytes).map_err(|e| {
                    StructuralCompatError::Manifest(format!(
                        "{}: invalid tag JSON: {e:?}",
                        entry.name
                    ))
                })?;
                let mut feature = FeatureUnit::new(
                    format!("tag:{namespace}:{stem}"),
                    FeatureKind::Data,
                    &entry.name,
                );
                feature.referenced_ids = collect_tag_references(&parsed);
                feature
                    .required_capabilities
                    .push(CapabilityRequirement::inferred_datapack(
                        "datapack",
                        None,
                        false,
                        &entry.name,
                    ));
                unit.features.push(feature);
            }
            "worldgen" | "dimension" | "dimension_type" => {
                let bytes = read_zip_member(jar_bytes, entry, display)
                    .map_err(|e| StructuralCompatError::Archive(e.reason().to_string()))?;
                let parsed = parse_json(&bytes).map_err(|e| {
                    StructuralCompatError::Manifest(format!(
                        "{}: invalid worldgen JSON: {e:?}",
                        entry.name
                    ))
                })?;
                let mut feature = FeatureUnit::new(
                    format!("worldgen:{namespace}:{stem}"),
                    FeatureKind::Worldgen,
                    &entry.name,
                );
                feature.affects_worldgen = true;
                feature.referenced_ids = references_for_keys(
                    &parsed,
                    &[
                        "biome",
                        "biomes",
                        "feature",
                        "features",
                        "block",
                        "blocks",
                        "state",
                        "target",
                        "fluid",
                        "structure",
                        "template_pool",
                        "processor_list",
                    ],
                );
                feature
                    .required_capabilities
                    .push(CapabilityRequirement::inferred_datapack(
                        "worldgen",
                        None,
                        false,
                        &entry.name,
                    ));
                unit.features.push(feature);
            }
            _ => {}
        }
    }
    Ok(())
}

fn references_for_keys(value: &JsonValue, keys: &[&str]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    collect_keyed_references(value, keys, &mut out);
    out
}

fn collect_keyed_references(value: &JsonValue, keys: &[&str], out: &mut BTreeSet<String>) {
    match value {
        JsonValue::Object(obj) => {
            for (key, child) in obj {
                if matches!(key.as_str(), "type" | "tag") {
                    continue;
                }
                if keys.contains(&key.as_str()) {
                    collect_strings_deep(child, out);
                } else {
                    collect_keyed_references(child, keys, out);
                }
            }
        }
        JsonValue::Array(items) => {
            for item in items {
                collect_keyed_references(item, keys, out);
            }
        }
        _ => {}
    }
}

fn collect_tag_references(root: &JsonValue) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let Some(values) = root
        .as_object()
        .and_then(|obj| obj.get("values"))
        .and_then(JsonValue::as_array)
    else {
        return out;
    };
    for value in values {
        match value {
            JsonValue::String(id) => {
                if let Some(id) = normalize_resource_reference(id) {
                    out.insert(id);
                }
            }
            JsonValue::Object(obj) => {
                let required = obj
                    .get("required")
                    .and_then(JsonValue::as_bool)
                    .unwrap_or(true);
                if required {
                    if let Some(id) = obj
                        .get("id")
                        .and_then(JsonValue::as_str)
                        .and_then(normalize_resource_reference)
                    {
                        out.insert(id);
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn recipe_is_optional(root: &JsonValue) -> bool {
    let Some(obj) = root.as_object() else {
        return false;
    };
    obj.get("type").and_then(JsonValue::as_str) == Some("neoforge:conditional")
        || obj.contains_key("neoforge:conditions")
        || obj.contains_key("fabric:load_conditions")
        || obj.contains_key("conditions")
        || obj.get("optional").and_then(JsonValue::as_bool) == Some(true)
}

fn collect_recipe_references(value: &JsonValue, out: &mut BTreeSet<String>) {
    const REFERENCE_KEYS: &[&str] = &[
        "item",
        "items",
        "id",
        "result",
        "results",
        "output",
        "outputs",
        "ingredient",
        "ingredients",
        "input",
        "inputs",
        "item_input",
        "fluid_input",
        "input_item",
        "input_fluid",
        "base",
        "addition",
        "template",
        "key",
        "block",
        "fluid",
        "entity",
    ];
    let Some(obj) = value.as_object() else { return };
    for (key, child) in obj {
        if matches!(key.as_str(), "type" | "group" | "category" | "tag") {
            continue;
        }
        if REFERENCE_KEYS.contains(&key.as_str()) {
            collect_strings_deep(child, out);
        } else {
            collect_recipe_references_child(child, out);
        }
    }
}

fn collect_recipe_references_child(value: &JsonValue, out: &mut BTreeSet<String>) {
    match value {
        JsonValue::Object(_) => collect_recipe_references(value, out),
        JsonValue::Array(items) => {
            for item in items {
                collect_recipe_references_child(item, out);
            }
        }
        _ => {}
    }
}

fn collect_strings_deep(value: &JsonValue, out: &mut BTreeSet<String>) {
    match value {
        JsonValue::String(text) => {
            if let Some(id) = normalize_resource_reference(text) {
                out.insert(id);
            }
        }
        JsonValue::Array(items) => {
            for item in items {
                collect_strings_deep(item, out);
            }
        }
        JsonValue::Object(obj) => {
            for (key, child) in obj {
                if key != "type" && key != "tag" {
                    collect_strings_deep(child, out);
                }
            }
        }
        _ => {}
    }
}

fn normalize_resource_reference(text: &str) -> Option<String> {
    if text.starts_with('#') {
        return None;
    }
    ResourceLocation::parse(text).ok()?;
    if text.contains(':') {
        Some(text.to_string())
    } else {
        Some(format!("minecraft:{text}"))
    }
}

fn scan_bytecode_capabilities(
    jar_bytes: &[u8],
    display: &str,
    entries: &[crate::modpack::ZipMemberEntry],
    unit: &mut InspectableModUnit,
) -> Result<(), StructuralCompatError> {
    let mut found: BTreeMap<String, String> = BTreeMap::new();
    let mut cosmetic_evidence = Vec::new();
    let mut classes = 0usize;
    let mut total_bytes = 0u64;
    for entry in entries
        .iter()
        .filter(|entry| entry.name.ends_with(".class"))
    {
        classes += 1;
        if classes > MAX_BYTECODE_MEMBERS {
            return Err(StructuralCompatError::Validation(format!(
                "{display}: class member count exceeds {MAX_BYTECODE_MEMBERS}"
            )));
        }
        let bytes = read_zip_member(jar_bytes, entry, display)
            .map_err(|e| StructuralCompatError::Archive(e.reason().to_string()))?;
        total_bytes = total_bytes.saturating_add(bytes.len() as u64);
        if total_bytes > MAX_BYTECODE_BYTES {
            return Err(StructuralCompatError::Validation(format!(
                "{display}: class bytecode exceeds {MAX_BYTECODE_BYTES} bytes"
            )));
        }
        for symbol in class_utf8_symbols(&bytes).map_err(|reason| {
            StructuralCompatError::Validation(format!("{}: {reason}", entry.name))
        })? {
            if let Some(capability) = bytecode_capability(&symbol) {
                found
                    .entry(capability.to_string())
                    .or_insert(symbol.clone());
            }
            if is_client_cosmetic_symbol(&symbol) {
                cosmetic_evidence.push(symbol);
            }
        }
    }
    for (capability, symbol) in found {
        if capability == "client_hud" {
            continue;
        }
        if capability == "worldgen" {
            unit.modifies_worldgen = true;
        }
        unit.add_inferred(CapabilityRequirement::inferred_bytecode(
            capability, None, false, symbol,
        ));
    }
    if !cosmetic_evidence.is_empty() {
        cosmetic_evidence.sort();
        cosmetic_evidence.dedup();
        let mut feature = FeatureUnit::new(
            "client_cosmetic:bytecode",
            FeatureKind::ClientCosmetic,
            cosmetic_evidence.join(","),
        );
        feature.optional = true;
        feature.isolated = true;
        feature.affects_core_state = false;
        feature.cosmetic_only = true;
        feature
            .required_capabilities
            .push(CapabilityRequirement::inferred_bytecode(
                "client_hud",
                None,
                true,
                cosmetic_evidence[0].clone(),
            ));
        unit.features.push(feature);
    }
    Ok(())
}

fn bytecode_capability(symbol: &str) -> Option<&'static str> {
    let s = symbol.to_ascii_lowercase();
    if s.contains("mana") {
        Some("mana_bus")
    } else if s.contains("energy")
        && (s.contains("capabilit") || s.contains("transfer") || s.contains("storage"))
    {
        Some("energy_bus")
    } else if s.contains("fluid")
        && (s.contains("capabilit")
            || s.contains("transfer")
            || s.contains("handler")
            || s.contains("tank"))
    {
        Some("fluid_bus")
    } else if s.contains("itemhandler")
        || s.contains("itemstackhandler")
        || (s.contains("transfer") && s.contains("item"))
    {
        Some("item_bus")
    } else if s.contains("lootmodifier")
        || s.contains("loot/modifier")
        || s.contains("loot_modifier")
    {
        Some("loot_modifiers")
    } else if s.contains("worldgen")
        || s.contains("world/generation")
        || s.contains("structurepiece")
        || s.contains("biomemodifier")
    {
        Some("worldgen")
    } else if s.contains("recipe") {
        Some("recipe")
    } else if s.contains("entity")
        && (s.contains("register") || s.contains("registry") || s.contains("deferredregister"))
    {
        Some("entity_registry")
    } else if s.contains("block")
        && (s.contains("register") || s.contains("registry") || s.contains("deferredregister"))
    {
        Some("block_registry")
    } else {
        None
    }
}

fn is_client_cosmetic_symbol(symbol: &str) -> bool {
    let s = symbol.to_ascii_lowercase();
    s.contains("hud") || s.contains("tooltip") || s.contains("overlay")
}

fn scan_wasm_imports(
    jar_bytes: &[u8],
    display: &str,
    entries: &[crate::modpack::ZipMemberEntry],
    unit: &mut InspectableModUnit,
) -> Result<(), StructuralCompatError> {
    for entry in entries.iter().filter(|entry| entry.name.ends_with(".wasm")) {
        let bytes = read_zip_member(jar_bytes, entry, display)
            .map_err(|e| StructuralCompatError::Archive(e.reason().to_string()))?;
        let imports = parse_wasm_imports(&bytes).map_err(|reason| {
            StructuralCompatError::Validation(format!("{}: {reason}", entry.name))
        })?;
        for import in imports {
            let evidence = format!("{}:{}:{}", entry.name, import.module, import.name);
            match import.name.as_str() {
                "host_log" => {}
                "host_get_block" | "host_set_block" | "megu_bump_block" | "megu_section_rev"
                | "megu_chunk_rev" => {
                    unit.add_inferred(CapabilityRequirement::inferred_wasm_import(
                        "block_registry",
                        None,
                        false,
                        &import.module,
                        &import.name,
                    ));
                }
                "host_emit_chat" => unit.add_inferred(CapabilityRequirement::inferred_wasm_import(
                    "chat",
                    None,
                    false,
                    &import.module,
                    &import.name,
                )),
                "megu_ui_mark_dirty" => {
                    let mut feature = FeatureUnit::new(
                        format!("client_cosmetic:wasm:{}", import.name),
                        FeatureKind::ClientCosmetic,
                        evidence,
                    );
                    feature.optional = true;
                    feature.isolated = true;
                    feature.affects_core_state = false;
                    feature.cosmetic_only = true;
                    feature.required_capabilities.push(
                        CapabilityRequirement::inferred_wasm_import(
                            "client_hud",
                            None,
                            true,
                            &import.module,
                            &import.name,
                        ),
                    );
                    unit.features.push(feature);
                }
                unknown => {
                    let mut sanitized: String = unknown
                        .chars()
                        .map(|c| {
                            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' {
                                c
                            } else {
                                '_'
                            }
                        })
                        .collect();
                    if sanitized.is_empty() || !sanitized.as_bytes()[0].is_ascii_lowercase() {
                        sanitized = format!("wasm_{sanitized}");
                    }
                    unit.add_inferred(CapabilityRequirement::inferred_wasm_import(
                        format!("wasm_host_{sanitized}"),
                        None,
                        false,
                        &import.module,
                        &import.name,
                    ));
                }
            }
        }
    }
    Ok(())
}

fn scan_native_manifest(
    jar_bytes: &[u8],
    display: &str,
    entries: &[crate::modpack::ZipMemberEntry],
    unit: &mut InspectableModUnit,
) -> Result<(), StructuralCompatError> {
    let Some(entry) = entries.iter().find(|entry| entry.name == "megu-mod.toml") else {
        return Ok(());
    };
    let bytes = read_zip_member(jar_bytes, entry, display)
        .map_err(|e| StructuralCompatError::Archive(e.reason().to_string()))?;
    let text = String::from_utf8(bytes).map_err(|_| {
        StructuralCompatError::Manifest(format!("{display}: megu-mod.toml is not UTF-8"))
    })?;
    let mut in_capabilities = false;
    for raw_line in text.lines() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            in_capabilities = line == "[capabilities]";
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim().trim_matches('"').trim_matches('\'');
        if key == "worldgen_epoch" {
            if let Some(existing) = &unit.worldgen_epoch {
                if existing != value {
                    unit.core_schema_issues.push(CoreSchemaIssue {
                        surface: "conflicting_worldgen_epoch".to_string(),
                        evidence: "megu-mod.toml:worldgen_epoch".to_string(),
                        message: format!("conflicting worldgen epochs '{existing}' and '{value}'"),
                    });
                }
            } else {
                unit.worldgen_epoch = Some(value.to_string());
            }
        } else if in_capabilities && value == "true" {
            let (capability, optional) = match key {
                "read_blocks" | "write_blocks" => ("block_registry", false),
                "register_recipes" => ("recipe", false),
                "entity_events" => ("entity_registry", false),
                "ui_surface" => ("client_hud", true),
                "chat_broadcast" => ("chat", false),
                _ => continue,
            };
            let req = CapabilityRequirement::declared_field(
                capability,
                None,
                optional,
                format!("megu-mod.toml[capabilities].{key}"),
            );
            if optional {
                let mut feature = FeatureUnit::new(
                    "client_cosmetic:manifest_ui_surface",
                    FeatureKind::ClientCosmetic,
                    format!("megu-mod.toml:{key}"),
                );
                feature.optional = true;
                feature.isolated = true;
                feature.affects_core_state = false;
                feature.cosmetic_only = true;
                feature.required_capabilities.push(req);
                unit.features.push(feature);
            } else {
                unit.declared_requirements.push(req);
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Bytecode and WASM structure readers
// ---------------------------------------------------------------------------

fn class_utf8_symbols(bytes: &[u8]) -> Result<Vec<String>, String> {
    fn take<'a>(bytes: &'a [u8], pos: &mut usize, n: usize) -> Result<&'a [u8], String> {
        let end = pos.checked_add(n).ok_or("class offset overflow")?;
        let out = bytes
            .get(*pos..end)
            .ok_or("truncated class constant pool")?;
        *pos = end;
        Ok(out)
    }
    fn take_u16(bytes: &[u8], pos: &mut usize) -> Result<u16, String> {
        let wide: [u8; 2] = take(bytes, pos, 2)?
            .try_into()
            .map_err(|_| "truncated class constant pool".to_string())?;
        Ok(u16::from_be_bytes(wide))
    }

    if bytes.len() < 10 || bytes[..4] != [0xca, 0xfe, 0xba, 0xbe] {
        return Err("bad class magic".to_string());
    }
    let mut pos = 8usize;
    let cp_count = take_u16(bytes, &mut pos)? as usize;
    let mut utf8 = Vec::new();
    let mut index = 1usize;
    while index < cp_count {
        let tag = take(bytes, &mut pos, 1)?[0];
        match tag {
            1 => {
                let len = take_u16(bytes, &mut pos)? as usize;
                let payload = take(bytes, &mut pos, len)?;
                utf8.push(String::from_utf8_lossy(payload).into_owned());
            }
            3 | 4 => {
                take(bytes, &mut pos, 4)?;
            }
            5 | 6 => {
                take(bytes, &mut pos, 8)?;
                index += 1;
            }
            7 | 8 | 16 | 19 | 20 => {
                take(bytes, &mut pos, 2)?;
            }
            9 | 10 | 11 | 12 | 17 | 18 => {
                take(bytes, &mut pos, 4)?;
            }
            15 => {
                take(bytes, &mut pos, 3)?;
            }
            other => return Err(format!("unsupported constant-pool tag {other}")),
        }
        index += 1;
    }
    utf8.sort();
    utf8.dedup();
    Ok(utf8)
}

#[derive(Debug)]
struct WasmImport {
    module: String,
    name: String,
}

fn parse_wasm_imports(bytes: &[u8]) -> Result<Vec<WasmImport>, String> {
    if bytes.len() < 8 || bytes[..4] != *b"\0asm" {
        return Err("bad WASM magic".to_string());
    }
    if bytes[4..8] != [1, 0, 0, 0] {
        return Err("unsupported WASM version".to_string());
    }
    let mut pos = 8usize;
    let mut imports = Vec::new();
    let mut saw_import_section = false;
    while pos < bytes.len() {
        let section_id = *bytes.get(pos).ok_or("truncated WASM section id")?;
        pos += 1;
        let size = read_leb_u32(bytes, &mut pos)? as usize;
        let end = pos.checked_add(size).ok_or("WASM section size overflow")?;
        if end > bytes.len() {
            return Err("truncated WASM section".to_string());
        }
        if section_id == 2 {
            if saw_import_section {
                return Err("duplicate WASM import section".to_string());
            }
            saw_import_section = true;
            let mut p = pos;
            let count = read_leb_u32(bytes, &mut p)?;
            for _ in 0..count {
                let module = read_wasm_name(bytes, &mut p, end)?;
                let name = read_wasm_name(bytes, &mut p, end)?;
                if p >= end {
                    return Err("truncated WASM import descriptor".to_string());
                }
                let kind = bytes[p];
                p += 1;
                match kind {
                    0 => {
                        read_leb_u32(bytes, &mut p)?;
                    }
                    1 => {
                        if p >= end {
                            return Err("truncated WASM table import".to_string());
                        }
                        p += 1;
                        skip_wasm_limits(bytes, &mut p, end)?;
                    }
                    2 => skip_wasm_limits(bytes, &mut p, end)?,
                    3 => {
                        if p + 2 > end {
                            return Err("truncated WASM global import".to_string());
                        }
                        p += 2;
                    }
                    4 => {
                        if p >= end {
                            return Err("truncated WASM tag import".to_string());
                        }
                        p += 1;
                        read_leb_u32(bytes, &mut p)?;
                    }
                    other => return Err(format!("unknown WASM import kind {other}")),
                }
                imports.push(WasmImport { module, name });
            }
            if p != end {
                return Err("WASM import section length mismatch".to_string());
            }
        }
        pos = end;
    }
    Ok(imports)
}

fn read_leb_u32(bytes: &[u8], pos: &mut usize) -> Result<u32, String> {
    let mut value = 0u32;
    for shift in (0..35).step_by(7) {
        let byte = *bytes.get(*pos).ok_or("truncated LEB128")?;
        *pos += 1;
        value |= u32::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err("oversized LEB128".to_string())
}

fn read_wasm_name(bytes: &[u8], pos: &mut usize, end: usize) -> Result<String, String> {
    let len = read_leb_u32(bytes, pos)? as usize;
    let stop = pos.checked_add(len).ok_or("WASM name length overflow")?;
    if stop > end {
        return Err("truncated WASM import name".to_string());
    }
    let text = std::str::from_utf8(&bytes[*pos..stop])
        .map_err(|_| "WASM import name is not UTF-8".to_string())?
        .to_string();
    *pos = stop;
    Ok(text)
}

fn skip_wasm_limits(bytes: &[u8], pos: &mut usize, end: usize) -> Result<(), String> {
    if *pos >= end {
        return Err("truncated WASM limits".to_string());
    }
    let flags = bytes[*pos];
    *pos += 1;
    read_leb_u32(bytes, pos)?;
    match flags {
        0 => {}
        1 | 3 => {
            read_leb_u32(bytes, pos)?;
        }
        other => return Err(format!("unsupported WASM limits flags {other}")),
    }
    Ok(())
}
